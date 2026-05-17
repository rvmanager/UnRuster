//! Small helpers shared by analysis collectors.

use rustc_middle::ty::TyCtxt;
use rustc_span::Span;

pub fn span_to_file_line(tcx: TyCtxt<'_>, span: Span) -> (String, u32) {
    if span.is_dummy() {
        return ("<unknown>".into(), 0);
    }
    let sm = tcx.sess.source_map();
    let loc = sm.lookup_char_pos(span.lo());
    let file = file_name_to_string(&loc.file.name);
    (file, loc.line as u32)
}

fn file_name_to_string(fname: &rustc_span::FileName) -> String {
    match fname {
        rustc_span::FileName::Real(real) => real
            .local_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{:?}", real)),
        other => format!("{:?}", other),
    }
}
