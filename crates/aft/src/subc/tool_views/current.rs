use serde_json::json;

use super::Shipped;

pub(super) const CATALOG_DIGEST: &str =
    "sha256:af4559652b832b8908f4dc39364874eea6950ab708567e70f17d304e8c0f9a32";

pub(super) fn append(entry: &mut Shipped) {
    let mut manifest = entry.history.last().expect("shipped view").clone();
    let sources = match entry.tool.name.as_str() {
        "edit" => Some([include_str!("edit.row.ui"), include_str!("edit.page.ui")]),
        "bash" => Some([include_str!("bash.row.ui"), include_str!("bash.page.ui")]),
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
