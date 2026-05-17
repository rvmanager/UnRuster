use rustc_middle::ty::TyCtxt;
use rustc_span::Span;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize)]
pub enum Severity {
    Warn,
    Note,
}

#[derive(Debug)]
pub struct Diagnostic {
    pub lint: &'static str,
    pub severity: Severity,
    pub message: String,
    pub span: Span,
    pub help: Option<String>,
}

#[derive(Default)]
pub struct Reporter {
    pub diagnostics: Vec<Diagnostic>,
}

impl Reporter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn warn(&mut self, lint: &'static str, span: Span, message: impl Into<String>) -> &mut Self {
        self.diagnostics.push(Diagnostic {
            lint,
            severity: Severity::Warn,
            message: message.into(),
            span,
            help: None,
        });
        self
    }

    pub fn with_help(&mut self, help: impl Into<String>) -> &mut Self {
        if let Some(last) = self.diagnostics.last_mut() {
            last.help = Some(help.into());
        }
        self
    }

    pub fn emit(self, tcx: TyCtxt<'_>) {
        let dcx = tcx.dcx();
        for d in self.diagnostics {
            let mut diag = dcx.struct_span_warn(d.span, format!("[{}] {}", d.lint, d.message));
            if let Some(help) = d.help {
                diag.help(help);
            }
            diag.emit();
        }
    }
}
