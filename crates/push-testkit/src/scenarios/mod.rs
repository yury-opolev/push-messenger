//! The scenarios, one module per area, and their case lists.

use crate::{Backend, Case};

mod devices;

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    let modules: Vec<(&str, Vec<Case<B>>)> = vec![("devices", devices::cases())];
    modules
        .into_iter()
        .flat_map(|(module, cases)| cases.into_iter().map(move |case| case.within(module)))
        .collect()
}
