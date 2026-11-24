use serde::{Deserialize, Serialize};

/// Parameters for `textDocument/diagnostic` pull requests.
///
/// This mirrors `lsp_types::DocumentDiagnosticParams`, but omits absent
/// optional fields from the JSON payload. The LSP 3.17 schema allows
/// `identifier` and `previousResultId` to be strings or absent, not `null`.
#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AftDocumentDiagnosticParams {
    /// The text document.
    pub text_document: lsp_types::TextDocumentIdentifier,

    /// The additional identifier provided during registration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,

    /// The result ID of a previous response if provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_result_id: Option<String>,

    #[serde(flatten)]
    pub work_done_progress_params: lsp_types::WorkDoneProgressParams,

    #[serde(flatten)]
    pub partial_result_params: lsp_types::PartialResultParams,
}

#[derive(Debug)]
pub enum AftDocumentDiagnosticRequest {}

impl lsp_types::request::Request for AftDocumentDiagnosticRequest {
    type Params = AftDocumentDiagnosticParams;
    type Result = lsp_types::DocumentDiagnosticReportResult;
    const METHOD: &'static str = "textDocument/diagnostic";
}

/// Parameters for `workspace/diagnostic` pull requests.
///
/// This mirrors `lsp_types::WorkspaceDiagnosticParams`, but omits absent
/// optional fields from the JSON payload. The LSP 3.17 schema allows
/// `identifier` to be a string or absent, not `null`.
#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AftWorkspaceDiagnosticParams {
    /// The additional identifier provided during registration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,

    /// The currently known diagnostic reports with their previous result ids.
    pub previous_result_ids: Vec<lsp_types::PreviousResultId>,

    #[serde(flatten)]
    pub work_done_progress_params: lsp_types::WorkDoneProgressParams,

    #[serde(flatten)]
    pub partial_result_params: lsp_types::PartialResultParams,
}

#[derive(Debug)]
pub enum AftWorkspaceDiagnosticRequest {}

impl lsp_types::request::Request for AftWorkspaceDiagnosticRequest {
    type Params = AftWorkspaceDiagnosticParams;
    type Result = lsp_types::WorkspaceDiagnosticReportResult;
    const METHOD: &'static str = "workspace/diagnostic";
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn text_document_identifier() -> lsp_types::TextDocumentIdentifier {
        lsp_types::TextDocumentIdentifier {
            uri: lsp_types::Uri::from_str("file:///tmp/example.ts").expect("valid test uri"),
        }
    }

    #[test]
    fn document_diagnostic_params_omits_none_fields() {
        let params = AftDocumentDiagnosticParams {
            text_document: text_document_identifier(),
            identifier: None,
            previous_result_id: None,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let json = serde_json::to_value(&params).expect("serialize params");
        let object = json.as_object().expect("params object");
        assert!(!object.contains_key("identifier"));
        assert!(!object.contains_key("previousResultId"));
    }

    #[test]
    fn document_diagnostic_params_serializes_some_fields() {
        let params = AftDocumentDiagnosticParams {
            text_document: text_document_identifier(),
            identifier: Some("tsgo".to_string()),
            previous_result_id: Some("result-1".to_string()),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let json = serde_json::to_value(&params).expect("serialize params");
        assert_eq!(json["identifier"], "tsgo");
        assert_eq!(json["previousResultId"], "result-1");
    }

    #[test]
    fn workspace_diagnostic_params_omits_none_identifier() {
        let params = AftWorkspaceDiagnosticParams {
            identifier: None,
            previous_result_ids: Vec::new(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let json = serde_json::to_value(&params).expect("serialize params");
        let object = json.as_object().expect("params object");
        assert!(!object.contains_key("identifier"));
        assert_eq!(json["previousResultIds"], serde_json::json!([]));
    }

    #[test]
    fn workspace_diagnostic_params_serializes_some_identifier() {
        let params = AftWorkspaceDiagnosticParams {
            identifier: Some("tsgo".to_string()),
            previous_result_ids: Vec::new(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let json = serde_json::to_value(&params).expect("serialize params");
        assert_eq!(json["identifier"], "tsgo");
        assert_eq!(json["previousResultIds"], serde_json::json!([]));
    }
}
