//! Internationalisation via gettext. The language follows the system locale.
//! Source strings are in English (msgid); translations live in
//! `po/<lang>.po`, compiled to `<datadir>/locale/<lang>/LC_MESSAGES/aurveto.mo`.

use gettextrs::{bindtextdomain, setlocale, textdomain, LocaleCategory};
use std::path::PathBuf;

const DOMAIN: &str = "aurveto";

/// Initialise gettext. Call once at the startup of each binary.
pub fn init() {
    setlocale(LocaleCategory::LcAll, "");
    let dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("/usr/share"))
        .join("locale");
    let _ = bindtextdomain(DOMAIN, dir);
    let _ = textdomain(DOMAIN);
}

/// Translates a simple message.
pub fn tr(msgid: &str) -> String {
    gettextrs::gettext(msgid)
}

/// Translates, then sequentially replaces each `{}` with the given arguments.
pub fn trf(msgid: &str, args: &[String]) -> String {
    let mut s = gettextrs::gettext(msgid);
    for arg in args {
        if let Some(i) = s.find("{}") {
            s.replace_range(i..i + 2, arg);
        }
    }
    s
}

/// Translation macro.
/// - `t!("English text")` → simple message.
/// - `t!("Found {} items", n)` → positional interpolation of each `{}`.
#[macro_export]
macro_rules! t {
    ($id:literal) => { $crate::i18n::tr($id) };
    ($id:literal, $($arg:expr),+ $(,)?) => {
        $crate::i18n::trf($id, &[$(($arg).to_string()),+])
    };
}
