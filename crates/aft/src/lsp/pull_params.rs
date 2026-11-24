use lsp_types::{DocumentDiagnosticReport, DocumentDiagnosticReportResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

pub(super) fn decode_document_diagnostic_report(
    value: Value,
) -> serde_json::Result<DocumentDiagnosticReportResult> {
    let report = match value.get("kind") {
        // gopls v0.24.0 leaves the kind empty on full document reports.
        // Decode the full payload directly so invalid fields can't fall back to a partial.
        Some(kind) if kind.as_str() == Some("") => {
            DocumentDiagnosticReport::Full(serde_json::from_value(value)?)
        }
        // A tagged response is a final report, not an untagged partial result.
        Some(_) => serde_json::from_value(value)?,
        None => return serde_json::from_value(value),
    };
    Ok(DocumentDiagnosticReportResult::Report(report))
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

    fn full_report(body: &str) -> lsp_types::RelatedFullDocumentDiagnosticReport {
        let reply: Value = serde_json::from_str(body).expect("captured JSON-RPC reply");
        let DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(report)) =
            decode_document_diagnostic_report(reply["result"].clone()).expect("decodable result")
        else {
            panic!("expected a full document report");
        };
        report
    }

    #[test]
    fn gopls_broken_file_reply_is_a_full_error_report() {
        let report = full_report(
            r#"{"jsonrpc":"2.0","result":{"kind":"","items":[{"range":{"start":{"line":2,"character":17},"end":{"line":2,"character":33}},"severity":1,"code":"IncompatibleAssign","codeDescription":{"href":"https://pkg.go.dev/golang.org/x/tools/internal/typesinternal#IncompatibleAssign"},"source":"compiler","message":"cannot use \"not an integer\" (untyped string constant) as int value in variable declaration"}]},"id":2}"#,
        );
        let items = report.full_document_diagnostic_report.items;
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].severity,
            Some(lsp_types::DiagnosticSeverity::ERROR)
        );
    }

    #[test]
    fn gopls_clean_file_reply_is_a_full_empty_report() {
        let report = full_report(r#"{"jsonrpc":"2.0","result":{"kind":"","items":[]},"id":3}"#);
        assert!(report.full_document_diagnostic_report.items.is_empty());
    }

    #[test]
    fn malformed_document_diagnostic_reports_do_not_fall_back_to_partial() {
        use serde_json::json;

        for value in [
            json!({"kind":"bogus", "items":[]}),
            json!({"kind":""}),
            json!({"kind":"", "items":{}}),
            json!({"kind":"", "items":[{"message":"missing range"}]}),
            json!({"kind":"", "items":[], "resultId":42}),
            json!({"kind":"", "items":[], "relatedDocuments":{"file:///broken.go":{"kind":"full"}}}),
        ] {
            assert!(
                decode_document_diagnostic_report(value.clone()).is_err(),
                "{value}"
            );
        }
    }

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
