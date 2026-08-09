use std::fmt;

pub(crate) const TARGET: &str = "busybeaver";

pub(crate) fn error(code: &'static str, component: &'static str, details: fmt::Arguments<'_>) {
    log::error!(target: TARGET, "code={code} component={component} {details}");
}
