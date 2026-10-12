//! The scenarios, one module per area, and their case lists.

use crate::{Backend, Case};

mod contacts;
mod delete;
mod devices;
mod direct;
mod edit;
mod group_receipts;
mod group_settings;
mod groups;
mod history;
mod invites;
mod listings;
mod membership;
mod read_state;
mod receipts;
mod send;
mod sync;
mod visibility;

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    let modules: Vec<(&str, Vec<Case<B>>)> = vec![
        ("contacts", contacts::cases()),
        ("delete", delete::cases()),
        ("devices", devices::cases()),
        ("direct", direct::cases()),
        ("edit", edit::cases()),
        ("group_receipts", group_receipts::cases()),
        ("group_settings", group_settings::cases()),
        ("groups", groups::cases()),
        ("history", history::cases()),
        ("invites", invites::cases()),
        ("listings", listings::cases()),
        ("membership", membership::cases()),
        ("read_state", read_state::cases()),
        ("receipts", receipts::cases()),
        ("send", send::cases()),
        ("sync", sync::cases()),
        ("visibility", visibility::cases()),
    ];
    modules
        .into_iter()
        .flat_map(|(module, cases)| cases.into_iter().map(move |case| case.within(module)))
        .collect()
}
