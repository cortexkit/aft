use serde_json::json;

use super::{Shipped, BASH_SEMANTICS};

pub(in crate::subc) const CATALOG_DIGEST: &str =
    "sha256:6e3239281ade150b0935a2ca239ead0f7f922ab0fdeeffc470a19fc0fbf3ddb0";

pub(super) fn append(entry: &mut Shipped) {
    let mut manifest = entry.history.last().expect("shipped view").clone();
    // The file-tool views changed since revision 3. Bash still serves the
    // same sources on this catalog.
    let (revision_delta, sources) = match entry.tool.name.as_str() {
        "edit" => (
            3,
            Some([include_str!("edit.row.ui"), include_str!("edit.page.ui")]),
        ),
        "bash" => (
            2,
            Some([include_str!("bash.row.ui"), include_str!("bash.page.ui")]),
        ),
        "write" => (
            3,
            Some([include_str!("edit.row.ui"), include_str!("write.page.ui")]),
        ),
        "apply_patch" => (
            3,
            Some([
                include_str!("apply_patch.row.ui"),
                include_str!("apply_patch.page.ui"),
            ]),
        ),
        _ => (2, None),
    };
    for (index, slot) in ["row", "page"].into_iter().enumerate() {
        manifest.template[slot]["catalog"]["digest"] = json!(CATALOG_DIGEST);
        if let Some(sources) = sources {
            manifest.template[slot]["source"] = json!(sources[index]);
        }
    }
    manifest.template_revision += revision_delta;
    entry.history.push(manifest);
}

pub(super) fn bash_tail_view(older: &Shipped) -> Shipped {
    let mut tool = older.tool.clone();
    tool.semantics = BASH_SEMANTICS;
    let mut manifest = older.history.last().expect("shipped bash view").clone();
    manifest.template["row"]["source"] = json!(include_str!("bash-v2.row.ui"));
    manifest.template["page"]["source"] = json!(include_str!("bash-v2.page.ui"));
    manifest.template_revision = 2;
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
