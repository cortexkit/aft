use std::{collections::BTreeMap, sync::LazyLock};

use cortexkit_role_tool_display::notice_view_key;
use cortexkit_role_tool_provider::views::{NoticeView, NoticeViewDefinition, NoticeViewTemplates};
use serde::Deserialize;
use serde_json::Value;

use super::Kind;
use crate::subc::tool_views::current::redeclare;

#[derive(Deserialize)]
struct Published {
    data_schema: Value,
    template_revision: u64,
    template: NoticeViewTemplates,
}

static VIEWS: LazyLock<BTreeMap<String, NoticeView>> = LazyLock::new(|| {
    // The three notice kinds share the same published schema and templates.
    let mut published: Published =
        serde_json::from_str(include_str!("shipped.json")).expect("committed notice view");
    redeclare(&mut published.template.row);
    redeclare(&mut published.template.page);
    Kind::ALL
        .into_iter()
        .map(|kind| {
            let pin =
                notice_view_key("aft", kind.name(), Kind::VERSION).expect("shipped notice pin");
            let view = NoticeView::new(
                NoticeViewDefinition::new(
                    kind.name(),
                    Kind::VERSION,
                    published.data_schema.clone(),
                ),
                published.template.clone(),
                published.template_revision + 1,
            );
            view.check_pin("aft", &pin).expect("shipped notice view");
            (pin, view)
        })
        .collect()
});

pub(in crate::subc) fn view_pins() -> impl Iterator<Item = &'static String> {
    VIEWS.keys()
}

pub(in crate::subc) fn view(pin: &str) -> Option<NoticeView> {
    VIEWS.get(pin).cloned()
}
