use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Invocation {
    pub(crate) args: Vec<String>,
    pub(crate) body_inputs: BTreeMap<String, String>,
}
