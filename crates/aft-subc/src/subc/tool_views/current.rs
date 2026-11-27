use serde_json::json;

use super::Shipped;

pub(in crate::subc) const CATALOG_DIGEST: &str =
    "sha256:f144092fdccf4813f84f84f034356532bc6770e7ae63dce3bf0fc2f7600f20d3";

pub(in crate::subc) fn redeclare(template: &mut serde_json::Value) {
    template["catalog"]["digest"] = json!(CATALOG_DIGEST);
}

pub(super) fn append(entry: &mut Shipped) {
    let mut manifest = entry.history.last().expect("shipped view").clone();
    for slot in ["row", "page"] {
        redeclare(&mut manifest.template[slot]);
    }
    manifest.template_revision += 1;
    entry.history.push(manifest);
}

pub(super) fn append_bash_row(entry: &mut Shipped) {
    let mut manifest = entry.history.last().expect("redeclared bash view").clone();
    manifest.template["row"]["source"] = json!(with_bash_title(include_str!("bash.row.ui")));
    manifest.template_revision += 1;
    entry.history.push(manifest);
}

pub(in crate::subc) fn with_bash_title(template: &str) -> String {
    // Sessions and apps pin the bash row view's template digest. Expanding the
    // shared title in place leaves the row's hashed template source byte-identical.
    let prelude = include_str!("bash-title.ui");
    let body = template.replace(
        "$BASH_TITLE",
        "input.description ?? (hasRest ? rest : input.command)",
    );
    format!("{prelude}\n{body}")
}
