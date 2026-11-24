use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::lsp::client::LspClient;
use crate::lsp::manager::settled_lsp;
use crate::lsp::registry::ServerKind;

pub struct IdleLspFixture {
    pub(crate) ctx: std::sync::Arc<crate::context::AppContext>,
    pub(crate) config: Config,
    pub(crate) busy: PathBuf,
    pub(crate) idle: PathBuf,
    _tmp: tempfile::TempDir,
}

impl IdleLspFixture {
    pub fn with_server_binary(server_binary: impl FnOnce() -> PathBuf) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config {
            project_root: Some(tmp.path().to_path_buf()),
            lsp_idle_minutes: 5,
            ..Config::default()
        };
        let ctx = std::sync::Arc::new(crate::context::AppContext::new(
            crate::context::default_language_provider_factory(),
            config.clone(),
        ));
        ctx.lsp()
            .override_binary(ServerKind::TypeScript(None), server_binary());
        ctx.lsp().set_extra_env("AFT_FAKE_LSP_IGNORE_SHUTDOWN", "1");
        ctx.lsp().set_extra_env("AFT_FAKE_LSP_REPORT_SHUTDOWN", "1");
        let busy = tmp.path().join("busy/index.ts");
        let idle = tmp.path().join("idle/index.ts");
        for (index, file) in [&busy, &idle].into_iter().enumerate() {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file.parent().unwrap().join("tsconfig.json"), "{}").unwrap();
            // Distinct SDKs retain two clients even with a shared repository root.
            let sdk = file.parent().unwrap().join("node_modules/typescript");
            std::fs::create_dir_all(&sdk).unwrap();
            std::fs::write(
                sdk.join("package.json"),
                format!(r#"{{"version":"5.9.{index}"}}"#),
            )
            .unwrap();
            std::fs::write(file, "export const n = 1;\n").unwrap();
            assert_eq!(
                settled_lsp(&mut ctx.lsp(), |manager| {
                    manager.ensure_server_for_file(file, &config)
                })
                .len(),
                1
            );
            ctx.lsp()
                .client_for_file_mut(file, &config)
                .unwrap()
                .set_last_used_at_for_test(Instant::now() - Duration::from_secs(301));
        }
        let fixture = Self {
            ctx,
            config,
            busy,
            idle,
            _tmp: tmp,
        };
        fixture.hover(&fixture.busy);
        fixture
    }

    pub fn hover(&self, file: &Path) {
        self.ctx.note_request();
        let hover = self
            .ctx
            .lsp()
            .client_for_file_mut(file, &self.config)
            .unwrap()
            .send_request::<lsp_types::request::HoverRequest>(idle_hover_params(file))
            .unwrap();
        assert!(hover.is_some());
    }

    pub fn pid(&self, file: &Path) -> Option<u32> {
        self.ctx
            .lsp()
            .client_for_file(file, &self.config)
            .map(LspClient::child_pid)
    }

    pub fn assert_only_busy(&self) {
        assert!(
            self.pid(&self.busy).is_some(),
            "recently requested server must survive"
        );
        assert!(
            self.pid(&self.idle).is_none(),
            "idle package must be detached despite a busy root"
        );
    }
}

pub(super) fn idle_hover_params(file: &Path) -> lsp_types::HoverParams {
    serde_json::from_value(json!({
        "textDocument": { "uri": url::Url::from_file_path(file).unwrap().as_str() },
        "position": { "line": 0, "character": 0 }
    }))
    .unwrap()
}
