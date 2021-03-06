use std::cmp::Ordering;
use std::cmp::Ordering::*;
use std::collections::{BTreeSet, HashMap};
use std::collections::hash_map::Entry::*;
use std::rc::Rc;
use core::{UntypedVar, State, FollowRef, VarWrapper, Unifier, StateInner};
use core::ExactVarRef::*;
use iter::{StateIter, single, TailIter};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct CountedVar(UntypedVar, usize);
impl Ord for CountedVar {
    fn cmp(&self, other: &CountedVar) -> Ordering {
        match self.1.cmp(&other.1) {
            Equal => self.0.cmp(&other.0),
            other => other,
        }
    }
}
impl PartialOrd for CountedVar {
    fn partial_cmp(&self, other: &CountedVar) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

type VarWrapperIter = Box<Iterator<Item=Box<VarWrapper>>>;

fn value_iter(state: Rc<StateInner>, var: UntypedVar, mut iter: VarWrapperIter) -> TailIter {
    use iter::{TailIterResult, wrap_fn};
    wrap_fn(move || {
        while let Some(x) = iter.next() {
            let mut child = State::with_parent(state.clone());
            let tid = x.get_type_id();
            let newid = child.eqs.store_value_untyped(Exactly(x, tid));
            child.untyped_unify(newid, var, tid, true);
            if child.ok() { return TailIterResult(Some(child), Some(value_iter(state, var, iter))); }
        }
        return TailIterResult(None, None);
    })
}

struct ParentStateIter<'a> {
    state: Option<&'a StateInner>,
}

impl<'a> ParentStateIter<'a> {
    fn new(state: &'a State) -> ParentStateIter {
        ParentStateIter { state: Some(&**state) }
    }
}

impl<'a> Iterator for ParentStateIter<'a> {
    type Item = &'a StateInner;
    fn next(&mut self) -> Option<&'a StateInner> {
        let result = self.state;
        self.state = self.state.and_then(|s| {
            s.parent.as_ref().map(|x| &**x)
        });
        result
    }
}

struct GatheredValues {
    counted: BTreeSet<CountedVar>,
    vars: HashMap<UntypedVar, usize>,
}

impl GatheredValues {
    fn new<'a, I>(state: &State, in_vars: I) -> GatheredValues
    where I: IntoIterator<Item=(UntypedVar, Option<&'a VarWrapper>)> {
        let mut counted = BTreeSet::new();
        let mut vars = HashMap::new();

        for (var, val) in in_vars {
            let var = state.follow_id(var);
            //let val = state.get_ref(var);
            if let Some(val) = val {
                if val.value_count() > 1 && !vars.contains_key(&var) {
                    counted.insert(CountedVar(var, val.value_count()));
                    vars.insert(var, val.value_count());
                }
            }
        }
        GatheredValues { counted: counted, vars: vars }
    }
}

///! Assign a single fixed value to any variables, like `Fd`s, that may not have one.  This is an
///  important step prior to reification - without it, not all constraints can be verified, and you
///  may see incorrect results.
pub fn assign_all_values(state: State) -> StateIter {
    let gathered = {
        let iter = ParentStateIter::new(&state)
            .flat_map(|state| state.eqs.iter())
            .flat_map(|&(var, ref val)| match val.as_exact() {
                None => None.into_iter(),
                Some(x) => Some((var, unsafe { state.var_opt(x) })).into_iter(),
            });
        GatheredValues::new(&state, iter)
    };
    assign_values_inner(state, gathered.counted, gathered.vars)
}

///! Assign single fixed values to any set of variables, like `Fd`s, that may not have one.
pub fn assign_values<I>(state: State, in_vars: I) -> StateIter where I: IntoIterator<Item=UntypedVar> {
    let gathered = {
        let iter = in_vars.into_iter().map(|var| { (var, state.get_exact_val(var)) });
        GatheredValues::new(&state, iter)
    };
    assign_values_inner(state, gathered.counted, gathered.vars)
}

fn assign_values_inner(state: State, mut counted: BTreeSet<CountedVar>, mut vars: HashMap<UntypedVar, usize>) -> StateIter {
    use iter::TailIterResult;
    let var = match counted.iter().next() {
        Some(x) => *x,
        None => { return single(state) },
    };
    counted.remove(&var);
    vars.remove(&var.0);
    //println!("popping {:?} with {} values, {} or {} remain", var.0, var.1, counted.len(), vars.len());
    //let mut counted = counted.clone();
    //let mut vars = vars.clone();

    let val: Option<VarWrapperIter> = match state.get_exact_val(var.0) {
        Some(x) => {
            //println!("adding valueiter for {:?}", x);
            if x.value_count() == 1 { // should be impossible but oh well
                None
            } else {
                Some(x.value_iter())
            }
        },
        None => panic!("should be impossible!"),
    };
    let val = match val {
        Some(x) => x,
        None => { return assign_values_inner(state, counted, vars); },
    };
    let iter = TailIterResult(None, Some(value_iter(Rc::new(state.unwrap()), var.0, val)));
    
    iter
    .and(move |state| {
        //println!("iterating over {:?} with value {:?}", var.0, state.follow_ref(var.0).1.opt().unwrap());
        let mut counted = counted.clone();
        let mut vars = vars.clone();
        for &(key, ref some_val) in state.eqs.iter() {
            let var_entry = vars.entry(key);
            match some_val.as_exact() {
                Some(exactval) => {
                    let val_opt = unsafe { state.var_opt(exactval) };
                    if let Some(val) = val_opt {
                        match var_entry {
                            Occupied(mut x) => {
                                //println!("{:?} old len: {}, new len: {}", key, x.get(), val.value_count());
                                if *x.get() != val.value_count() {
                                    counted.remove(&CountedVar(key, *x.get()));
                                    if val.value_count() > 1 {
                                        counted.insert(CountedVar(key, val.value_count()));
                                        x.insert(val.value_count());
                                    } else {
                                        x.remove();
                                    }
                                }
                            },
                            Vacant(x) => {
                                counted.insert(CountedVar(key, val.value_count()));
                                x.insert(val.value_count());
                            }
                        }
                    } else {
                        panic!("impossible!");
                    }
                },
                None => {
                    if let Occupied(x) = var_entry {
                        counted.remove(&CountedVar(key, *x.get()));
                        x.remove();
                    }
                },
            }
        }
        assign_values_inner(state, counted, vars)
    })
}
