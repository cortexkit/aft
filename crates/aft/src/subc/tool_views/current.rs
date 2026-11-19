use serde_json::json;

use super::{Shipped, BASH_SEMANTICS};

pub(super) const CATALOG_DIGEST: &str =
    "sha256:af4559652b832b8908f4dc39364874eea6950ab708567e70f17d304e8c0f9a32";

pub(super) fn append(entry: &mut Shipped) {
    let mut manifest = entry.history.last().expect("shipped view").clone();
    let sources = match entry.tool.name.as_str() {
        "edit" => Some([include_str!("edit.row.ui"), include_str!("edit.page.ui")]),
        "bash" => Some([
            include_str!("bash-v2.row.ui"),
            include_str!("bash-v2.page.ui"),
        ]),
        // The app's checker only accepts the current catalog digest. For
        // tools without a new source, refresh the digest and keep the view.
        _ => None,
    };
    for (index, slot) in ["row", "page"].into_iter().enumerate() {
        manifest.template[slot]["catalog"]["digest"] = json!(CATALOG_DIGEST);
        if let Some(sources) = sources {
            manifest.template[slot]["source"] = json!(sources[index]);
        }
    }
    manifest.template_revision += 1;
    entry.history.push(manifest);
}

pub(super) fn bash_tail_view(older: &Shipped) -> Shipped {
    let mut tool = older.tool.clone();
    tool.semantics = BASH_SEMANTICS;
    let mut manifest = older.history.last().expect("shipped bash view").clone();
    manifest.template["row"]["source"] = json!(include_str!("bash.row.ui"));
    manifest.template["page"]["source"] = json!(include_str!("bash.page.ui"));
    manifest.template_revision = 1;
    manifest.record_schema.revision += 1;
    manifest.record_schema.completion_fields.insert(
        "tail".into(),
        cortexkit_role_tool_display::FieldKind::TextLines,
    );
    Shipped {
        tool,
        history: vec![manifest],
    }
}
