//! GTK4 / libadwaita graphical interface for aurveto.
//!
//! Main view: the AUR updates (check + verdicts + apply).
//! The settings live in a separate dialog (gear button).
//!
//! The visual language follows the "AURVeto Redesign" mock: a warm off-white
//! canvas, a green brand accent, flat bordered cards, and a per-package
//! decision chain drawn as green-bulleted steps. The palette lives in
//! `THEME_CSS`; the widgets below only assign classes and lay out.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{glib, Adjustment, Orientation, StringList};
use libadwaita as adw;
use libadwaita::prelude::*;

use aurveto::config::{Config, DelayMode, Provider, Secrets};
use aurveto::pipeline::{self, ChainStep, Decision, Outcome, StepStatus};
use aurveto::{aur, deploy, scan, t};

const APP_ID: &str = "fr.xhelliom.AurVeto";

/// RGB color (0..1) of a ring segment / a legend dot.
type Rgb = (f64, f64, f64);
/// RGBA color (0..1) — a fill tint with its own alpha.
type Rgba = (f64, f64, f64, f64);

/// Diameter of the summary donut (px).
const RING_SIZE: i32 = 112;
/// Ring track & segment thickness at rest (px).
const RING_WIDTH: f64 = 11.0;
/// Segment thickness when focused through its legend entry (px).
const RING_FOCUS_WIDTH: f64 = 14.0;
/// Opacity of the non-focused segments while one is focused.
const RING_DIM: f64 = 0.32;
/// Gap between adjacent segments (radians), so they read as distinct arcs.
const RING_GAP: f64 = 0.10;
/// Side of a legend dot (px).
const DOT_SIZE: i32 = 9;
/// Side of the brand shield in the header (px).
const LOGO_SIZE: i32 = 30;
/// Side of a decision-chain step bullet (px).
const BULLET_SIZE: i32 = 18;
/// How many pending packages the "on hold" list shows before folding the rest
/// behind a "+N more" reveal — matches the mock's compact list.
const VISIBLE_WAITING: usize = 2;
/// AUR package providing the `aur-scan` binary the static scan delegates to.
const AUR_SCAN_PACKAGE: &str = "ks-aur-scanner";

// Donut / legend palette, matching the redesign: blue reads as "ready to
// install", orange as "maturing under the delay", red as "blocked".
const COLOR_ALLOW: Rgb = (0.184, 0.475, 0.859); // blue  #2f79db
const COLOR_DELAY: Rgb = (0.878, 0.569, 0.184); // orange #e0912f
const COLOR_BLOCK: Rgb = (0.784, 0.192, 0.184); // red    #c8312f

/// The whole theme: warm canvas, green accent, flat cards, decision-chain
/// bullets and grouped rows. Loaded once at startup (`install_css`). Colors are
/// literal sRGB so the sheet parses on any GTK4 build.
const THEME_CSS: &str = r#"
/* ---- canvas & header ---- */
window { background-color: #f6f4ef; }
headerbar { background: transparent; box-shadow: none; }

.ag-title { font-size: 15px; font-weight: 700; color: #161513; }
button.ag-gear { background: transparent; background-image: none; box-shadow: none; border: 1px solid rgba(20,20,18,0.10); border-radius: 8px; min-width: 30px; min-height: 30px; color: rgba(20,20,18,0.5); }
button.ag-gear:hover { background-color: rgba(20,20,18,0.05); }

/* ---- hero ---- */
.ag-hero { padding: 6px 4px 14px 4px; }
.ag-recap { font-size: 13px; color: rgba(20,20,18,0.62); }

button.ag-btn-check { background-color: #ffffff; background-image: none; color: #161513; border: 1px solid rgba(20,20,18,0.14); border-radius: 8px; padding: 7px 15px; font-weight: 500; box-shadow: none; }
button.ag-btn-check:hover { background-color: #faf9f6; }
button.ag-btn-primary { background-color: #3aa35c; background-image: none; color: #ffffff; border: none; border-radius: 8px; padding: 7px 16px; font-weight: 600; box-shadow: none; }
button.ag-btn-primary:hover { background-color: #349152; }

/* ---- section headers ---- */
button.ag-section { background: transparent; background-image: none; box-shadow: none; border: none; padding: 4px 2px; }
button.ag-section:hover { background: transparent; }
.ag-caret { font-size: 9px; color: rgba(20,20,18,0.35); }
.ag-section-label { font-size: 11px; font-weight: 700; color: rgba(20,20,18,0.42); letter-spacing: 1px; }
.ag-section-suffix { font-size: 10px; color: rgba(20,20,18,0.32); }

/* ---- package card ---- */
.ag-card { background-color: #ffffff; border: 1px solid rgba(20,20,18,0.09); border-radius: 12px; padding: 16px 18px; }
button.ag-toggler { background: transparent; background-image: none; box-shadow: none; border: none; padding: 0; }
button.ag-toggler:hover { background: transparent; }
.ag-pkg-name { font-size: 14px; font-weight: 600; color: #161513; }
.ag-pkg-ver { font-family: monospace; font-size: 12px; color: rgba(20,20,18,0.5); }
.ag-pkg-sub { font-size: 10px; color: rgba(20,20,18,0.34); }

.ag-pill-safe { background-color: rgba(58,163,92,0.15); color: #2c7a45; border-radius: 999px; padding: 4px 12px; font-size: 11px; font-weight: 600; }
.ag-pill-blocked { background-color: rgba(200,49,47,0.14); color: #b02a28; border-radius: 999px; padding: 4px 12px; font-size: 11px; font-weight: 600; }

button.ag-install { background-color: #161513; background-image: none; color: #ffffff; border: none; border-radius: 7px; padding: 7px 14px; font-weight: 600; box-shadow: none; }
button.ag-install:hover { background-color: #2c2a27; }

/* ---- decision chain ---- */
.ag-chain { border-top: 1px dashed rgba(20,20,18,0.13); padding-top: 14px; margin-top: 14px; }
.ag-step-name { font-size: 12px; font-weight: 500; color: rgba(20,20,18,0.72); }
.ag-step-note { font-size: 11px; color: rgba(20,20,18,0.42); }

/* ---- grouped rows (on hold / official) ---- */
.ag-list { background-color: rgba(20,20,18,0.07); border-radius: 10px; padding: 1px; }
.ag-row { background-color: #ffffff; padding: 11px 15px; }
.ag-row-title { font-size: 13px; font-weight: 500; color: #161513; }
.ag-row-sub { font-size: 10px; color: rgba(20,20,18,0.32); }
.ag-row-meta { font-size: 11px; color: rgba(20,20,18,0.45); }
button.ag-more { background-color: #ffffff; background-image: none; box-shadow: none; color: rgba(20,20,18,0.5); border: none; padding: 10px 15px; font-size: 12px; font-weight: 500; }
button.ag-more:hover { background-color: #faf9f6; }

.ag-dot { min-width: 8px; min-height: 8px; border-radius: 999px; }
.ag-dot-orange { background-color: #e0912f; }
.ag-dot-blue { background-color: #2f79db; }
.ag-dot-red { background-color: #c8312f; }
.ag-dot-grey { background-color: rgba(20,20,18,0.28); }

.ag-divider { background-color: rgba(20,20,18,0.08); min-height: 1px; }
"#;

fn main() -> glib::ExitCode {
    aurveto::i18n::init();
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn provider_index(p: Provider) -> u32 {
    match p {
        Provider::Groq => 0,
        Provider::Anthropic => 1,
        Provider::Openai => 2,
    }
}

fn provider_from_index(i: u32) -> Provider {
    match i {
        1 => Provider::Anthropic,
        2 => Provider::Openai,
        _ => Provider::Groq,
    }
}

fn provider_name(p: Provider) -> &'static str {
    match p {
        Provider::Groq => "Groq",
        Provider::Anthropic => "Anthropic",
        Provider::Openai => "OpenAI",
    }
}

// =====================================================================
// Main window: UPDATES
// =====================================================================

fn build_ui(app: &adw::Application) {
    // The mock is a light, warm design; force the light scheme so libadwaita's
    // named colors and the donut track resolve against a light background.
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);
    install_css();
    let cfg = Rc::new(RefCell::new(Config::load_or_init().unwrap_or_default()));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("aurveto")
        .default_width(620)
        .default_height(780)
        .build();

    // Flat header carrying the brand cluster (green check logo + name) on the
    // left and the settings gear on the right, blended into the canvas.
    let header = adw::HeaderBar::new();
    header.add_css_class("flat");
    header.set_title_widget(Some(&gtk::Label::new(None)));

    let brand = gtk::Box::new(Orientation::Horizontal, 10);
    brand.append(&brand_logo());
    let title = gtk::Label::new(Some("aurveto"));
    title.add_css_class("ag-title");
    brand.append(&title);
    header.pack_start(&brand);

    let settings_btn = gtk::Button::builder()
        .icon_name("emblem-system-symbolic")
        .css_classes(["ag-gear"])
        .tooltip_text(t!("Settings"))
        .build();
    header.pack_end(&settings_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);

    // The static scan is a no-op when its binary is absent, which otherwise only
    // shows up as an "unavailable" note buried in every decision chain. Surface
    // it once, with the install one click away.
    let scan_banner = adw::Banner::builder()
        .title(t!("aur-scan is not installed: the static scan cannot run"))
        .button_label(t!("Install"))
        .revealed(scan_binary_missing(&cfg.borrow()))
        .build();
    toolbar.add_top_bar(&scan_banner);

    let page = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(0)
        .margin_top(4)
        .margin_bottom(22)
        .margin_start(22)
        .margin_end(22)
        .build();

    // Summary hero (donut + recap + primary actions), built once. The check
    // only refills the donut and the recap sentence, so the buttons persist.
    let check_btn = gtk::Button::builder()
        .label(t!("Check"))
        .css_classes(["ag-btn-check"])
        .build();
    let upgrade_btn = gtk::Button::builder()
        .label(t!("Update everything"))
        .css_classes(["ag-btn-primary"])
        .tooltip_text(t!("Official repos (pacman -Syu) then safe AUR packages"))
        .build();

    let ring_holder = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .valign(gtk::Align::Center)
        .build();
    let recap_label = gtk::Label::builder()
        .use_markup(true)
        .wrap(true)
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["ag-recap"])
        .build();
    let hero_actions = gtk::Box::new(Orientation::Horizontal, 8);
    hero_actions.append(&check_btn);
    hero_actions.append(&upgrade_btn);
    let hero_right = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    hero_right.append(&recap_label);
    hero_right.append(&hero_actions);
    let hero = gtk::Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(24)
        .css_classes(["ag-hero"])
        .build();
    hero.append(&ring_holder);
    hero.append(&hero_right);
    page.append(&hero);

    // Divider between the hero and the categorized lists.
    let divider = gtk::Box::new(Orientation::Horizontal, 0);
    divider.add_css_class("ag-divider");
    divider.set_hexpand(true);
    divider.set_margin_top(4);
    divider.set_margin_bottom(16);
    page.append(&divider);

    // One collapsible section per category (blocked / to install / on hold /
    // official), rebuilt on every check.
    let results = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(18)
        .build();
    results.append(&card(&info_row(&t!("Click “Check” to run the analysis."))));
    page.append(&results);

    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&page)
        .build();
    toolbar.set_content(Some(&scroller));

    // Navigation stack: "updates" at the root; the settings are pushed as a
    // full-screen page (rather than a floating dialog).
    let updates_page = adw::NavigationPage::new(&toolbar, "aurveto");
    let nav = adw::NavigationView::new();
    nav.add(&updates_page);

    let overlay = adw::ToastOverlay::new();
    overlay.set_child(Some(&nav));
    window.set_content(Some(&overlay));

    // Gear button -> full-screen settings page.
    {
        let cfg = cfg.clone();
        let nav = nav.clone();
        let overlay = overlay.clone();
        settings_btn.connect_clicked(move |_| {
            nav.push(&build_settings_page(&cfg, &overlay));
        });
    }

    // Banner action: install the scanner through the configured AUR helper.
    {
        let cfg = cfg.clone();
        let overlay = overlay.clone();
        scan_banner.connect_button_clicked(move |_| {
            let helper = sh_quote(&cfg.borrow().helper);
            let _ = launch_in_terminal(&format!("{helper} -S --needed {AUR_SCAN_PACKAGE}"));
            overlay.add_toast(adw::Toast::new(&t!(
                "Installing {} in a terminal",
                AUR_SCAN_PACKAGE
            )));
        });
    }

    // A check re-reads whether the scanner appeared since the window opened.
    {
        let cfg = cfg.clone();
        let scan_banner = scan_banner.clone();
        check_btn.connect_clicked(move |_| {
            scan_banner.set_revealed(scan_binary_missing(&cfg.borrow()));
        });
    }

    wire_check(
        &cfg,
        &check_btn,
        &ring_holder,
        &recap_label,
        &results,
        &overlay,
    );
    wire_upgrade(&upgrade_btn, &overlay);

    window.present();

    // Automatic refresh at startup: we trigger the same check as the button,
    // without duplicating its logic.
    check_btn.emit_clicked();
}

/// Wires the "Check" button: background evaluation then display, with a reminder
/// of the number of (signed) official-repo updates at the top.
fn wire_check(
    cfg: &Rc<RefCell<Config>>,
    check_btn: &gtk::Button,
    ring_holder: &gtk::Box,
    recap_label: &gtk::Label,
    results: &gtk::Box,
    overlay: &adw::ToastOverlay,
) {
    let cfg = cfg.clone();
    let ring_holder = ring_holder.clone();
    let recap_label = recap_label.clone();
    let results = results.clone();
    let overlay = overlay.clone();
    let check_btn_outer = check_btn.clone();
    check_btn.connect_clicked(move |_| {
        check_btn_outer.set_sensitive(false);
        check_btn_outer.set_label(&t!("Checking…"));
        clear_box(&results);
        results.append(&card(&loading_row()));

        let snapshot = cfg.borrow().clone();
        let (tx, rx) = async_channel::bounded::<Result<(Vec<String>, Vec<Outcome>), String>>(1);
        std::thread::spawn(move || {
            let official = aur::official_updates();
            let res = pipeline::evaluate(&snapshot)
                .map(|o| (official, o))
                .map_err(|e| e.to_string());
            let _ = tx.send_blocking(res);
        });

        let cfg = cfg.clone();
        let ring_holder = ring_holder.clone();
        let recap_label = recap_label.clone();
        let results = results.clone();
        let overlay = overlay.clone();
        let check_btn_inner = check_btn_outer.clone();
        glib::spawn_future_local(async move {
            if let Ok(res) = rx.recv().await {
                match res {
                    Ok((official, outcomes)) => {
                        render(
                            &cfg.borrow(),
                            &ring_holder,
                            &recap_label,
                            &results,
                            &official,
                            &outcomes,
                            &overlay,
                        );
                    }
                    Err(e) => {
                        clear_box(&results);
                        results.append(&card(&info_row(&t!("Error: {}", e))));
                    }
                }
            }
            check_btn_inner.set_sensitive(true);
            check_btn_inner.set_label(&t!("Check"));
        });
    });
}

/// Refills the hero (donut + recap) and rebuilds the collapsible sections from
/// the verdicts. All the decision-making is already done by `pipeline`; we only
/// present and group.
fn render(
    cfg: &Config,
    ring_holder: &gtk::Box,
    recap_label: &gtk::Label,
    results: &gtk::Box,
    official: &[String],
    outcomes: &[Outcome],
    overlay: &adw::ToastOverlay,
) {
    let summary = pipeline::summarize(outcomes);
    clear_box(ring_holder);
    ring_holder.append(&summary_ring(&summary));
    recap_label.set_label(&recap_text(&summary, official.len()));

    clear_box(results);

    if official.is_empty() && outcomes.is_empty() {
        results.append(&card(&up_to_date_row(cfg)));
        return;
    }

    let now = aur::now_secs();

    // Blocked first (the most important), expanded.
    let blocked: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| matches!(o.decision, Decision::Blocked(_)))
        .collect();
    if !blocked.is_empty() {
        let (sec, content) = section(&t!("Blocked"), blocked.len(), "", true);
        for o in &blocked {
            content.append(&blocked_card(o));
        }
        results.append(&sec);
    }

    // To install, expanded. Each package is its own card showing the decision
    // chain; the first opens by default so the chain is visible at a glance.
    let allowed: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| o.decision == Decision::Allow)
        .collect();
    if !allowed.is_empty() {
        let (sec, content) = section(&t!("To install"), allowed.len(), "", true);
        for (i, o) in allowed.iter().enumerate() {
            content.append(&package_card(o, overlay, i == 0));
        }
        results.append(&sec);
    }

    // On hold: a compact grouped list with the countdown per package.
    let delayed: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| matches!(o.decision, Decision::Delayed(_)))
        .collect();
    if !delayed.is_empty() {
        let (sec, content) = section(&t!("On hold"), delayed.len(), "", true);
        content.append(&waiting_list(&delayed, now));
        results.append(&sec);
    }

    // Official repos: collapsed by default (out of aurveto's scope).
    if !official.is_empty() {
        let (sec, content) = section(
            &t!("Official repositories (signed)"),
            official.len(),
            &t!("outside review scope"),
            false,
        );
        content.append(&official_list(official));
        results.append(&sec);
    }
}

/// The static scan is enabled but its binary is missing: the guard silently
/// does nothing, so the banner offers to install it. A scan disabled on purpose
/// is the user's call and raises nothing.
fn scan_binary_missing(cfg: &Config) -> bool {
    cfg.use_aur_scan && !scan::available()
}

/// Installs a single AUR package by running `aurveto apply <name>` in a
/// terminal. The CLI re-evaluates the decision chain at install time: this
/// bypasses no guard, it only narrows to one package.
fn install_one(name: &str, overlay: &adw::ToastOverlay) {
    let cli = sh_quote(&deploy::cli_command());
    let _ = launch_in_terminal(&format!("{cli} apply {}", sh_quote(name)));
    overlay.add_toast(adw::Toast::new(&t!("Installing {} in a terminal", name)));
}

/// Wires the "Update everything" button: runs `aurveto upgrade` in a terminal
/// (official repos via pacman -Syu then safe AUR packages).
fn wire_upgrade(upgrade_btn: &gtk::Button, overlay: &adw::ToastOverlay) {
    let overlay = overlay.clone();
    upgrade_btn.connect_clicked(move |_| {
        let cli = sh_quote(&deploy::cli_command());
        let _ = launch_in_terminal(&format!("{cli} upgrade"));
        overlay.add_toast(adw::Toast::new(&t!("Full update started in a terminal")));
    });
}

// =====================================================================
// SETTINGS dialog
// =====================================================================

fn build_settings_page(
    cfg: &Rc<RefCell<Config>>,
    overlay: &adw::ToastOverlay,
) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();

    // --- General group ---
    let general = adw::PreferencesGroup::builder()
        .title(t!("Delay & helper"))
        .build();
    let delay_row = adw::SpinRow::builder()
        .title(t!("Security delay (days)"))
        .adjustment(&Adjustment::new(
            cfg.borrow().delay_days as f64,
            0.0,
            365.0,
            1.0,
            7.0,
            0.0,
        ))
        .build();
    let mode_row = adw::ComboRow::builder()
        .title(t!("Delay mode"))
        .subtitle(t!(
            "Lag: revision from N days ago · Hold: block recent updates"
        ))
        .model(&StringList::new(&[
            &t!("Lag (deferred)"),
            &t!("Hold (block)"),
        ]))
        .selected(u32::from(cfg.borrow().delay_mode == DelayMode::Hold))
        .build();
    let helper_row = adw::ComboRow::builder()
        .title(t!("AUR helper"))
        .model(&StringList::new(&["yay", "paru"]))
        .selected(if cfg.borrow().helper == "paru" { 1 } else { 0 })
        .build();
    let scan_row = adw::SwitchRow::builder()
        .title(t!("Static scan (aur-scan)"))
        .subtitle(t!("Delegates to aur-scan if installed"))
        .active(cfg.borrow().use_aur_scan)
        .build();
    general.add(&delay_row);
    general.add(&mode_row);
    general.add(&helper_row);
    general.add(&scan_row);

    // --- AI review group ---
    let ai = adw::PreferencesGroup::builder()
        .title(t!("AI review"))
        .build();
    let ai_row = adw::SwitchRow::builder()
        .title(t!("Enable AI review"))
        .active(cfg.borrow().ai.enabled)
        .build();
    let provider_row = adw::ComboRow::builder()
        .title(t!("Provider"))
        .model(&StringList::new(&["Groq", "Anthropic", "OpenAI"]))
        .selected(provider_index(cfg.borrow().ai.provider))
        .build();
    let model_row = adw::EntryRow::builder()
        .title(t!("Model (empty = provider default)"))
        .text(cfg.borrow().ai.model.as_str())
        .build();
    let key_row = adw::PasswordEntryRow::builder().build();
    let votes_row = adw::SpinRow::builder()
        .title(t!("Confirmation votes"))
        .subtitle(t!("Triggered only to confirm a block"))
        .adjustment(&Adjustment::new(
            cfg.borrow().ai.confirm_votes as f64,
            1.0,
            9.0,
            1.0,
            1.0,
            0.0,
        ))
        .build();
    refresh_key_row(&key_row, provider_from_index(provider_row.selected()));
    {
        // Update the key label when the provider changes.
        let key_row = key_row.clone();
        provider_row.connect_selected_notify(move |row| {
            refresh_key_row(&key_row, provider_from_index(row.selected()));
        });
    }
    ai.add(&ai_row);
    ai.add(&provider_row);
    ai.add(&model_row);
    ai.add(&key_row);
    ai.add(&votes_row);

    // --- Notifications group ---
    let notif = adw::PreferencesGroup::builder()
        .title(t!("Notifications"))
        .description(t!("Periodic desktop notification of pending updates"))
        .build();
    let notif_row = adw::SwitchRow::builder()
        .title(t!("Enable notifications"))
        .active(cfg.borrow().notify.enabled)
        .build();
    let interval_row = adw::SpinRow::builder()
        .title(t!("Check interval (hours)"))
        .adjustment(&Adjustment::new(
            cfg.borrow().notify.interval_hours as f64,
            1.0,
            168.0,
            1.0,
            6.0,
            0.0,
        ))
        .build();
    let silent_row = adw::SwitchRow::builder()
        .title(t!("Silent when up to date"))
        .active(cfg.borrow().notify.silent_when_up_to_date)
        .build();
    let test_row = adw::ActionRow::builder()
        .title(t!("Test notification"))
        .subtitle(t!("Send one right now to check it works"))
        .build();
    let test_btn = gtk::Button::builder()
        .label(t!("Send"))
        .valign(gtk::Align::Center)
        .build();
    test_btn.connect_clicked(|_| deploy::send_test_notification());
    test_row.add_suffix(&test_btn);
    notif.add(&notif_row);
    notif.add(&interval_row);
    notif.add(&silent_row);
    notif.add(&test_row);

    // --- Whitelist group ---
    let wl = build_whitelist_group(cfg);

    page.add(&general);
    page.add(&ai);
    page.add(&notif);
    page.add(&wl);

    let header = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    let nav_page = adw::NavigationPage::new(&toolbar, &t!("Settings"));

    // Save when leaving the settings page (back to home).
    {
        let cfg = cfg.clone();
        let overlay = overlay.clone();
        let provider_row = provider_row.clone();
        nav_page.connect_hidden(move |_| {
            let provider = provider_from_index(provider_row.selected());
            {
                let mut c = cfg.borrow_mut();
                c.delay_days = delay_row.value() as u64;
                c.delay_mode = if mode_row.selected() == 1 {
                    DelayMode::Hold
                } else {
                    DelayMode::Lag
                };
                c.helper = if helper_row.selected() == 1 {
                    "paru".into()
                } else {
                    "yay".into()
                };
                c.use_aur_scan = scan_row.is_active();
                c.ai.enabled = ai_row.is_active();
                c.ai.provider = provider;
                c.ai.model = model_row.text().trim().to_string();
                c.ai.confirm_votes = votes_row.value() as u32;
                c.notify.enabled = notif_row.is_active();
                c.notify.interval_hours = interval_row.value() as u64;
                c.notify.silent_when_up_to_date = silent_row.is_active();
            }

            // The typed key (if non-empty) goes into the 0600 secrets file.
            let typed = key_row.text().to_string();
            if !typed.trim().is_empty() {
                let mut secrets = Secrets::load();
                secrets.set(provider, Some(typed));
                if let Err(e) = secrets.save() {
                    overlay.add_toast(adw::Toast::new(&t!("Secrets error: {}", e)));
                }
            }

            let toast = match cfg.borrow().save() {
                Ok(_) => adw::Toast::new(&t!("Settings saved")),
                Err(e) => adw::Toast::new(&t!("Error: {}", e)),
            };
            toast.set_timeout(2);
            overlay.add_toast(toast);

            // Sync the notification systemd timer with the settings.
            if let Err(e) = deploy::apply_notify(&cfg.borrow().notify) {
                overlay.add_toast(adw::Toast::new(&t!("Notification setup error: {}", e)));
            }
        });
    }

    nav_page
}

/// Updates the API key row's label depending on the provider, indicating whether
/// a key is already available (env or secrets). Never pre-fills the key.
fn refresh_key_row(key_row: &adw::PasswordEntryRow, provider: Provider) {
    let env_set = std::env::var(provider.default_key_env())
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let file_set = Secrets::load().get(provider).is_some();
    let state = if env_set {
        t!("set via $ENV")
    } else if file_set {
        t!("already saved")
    } else {
        t!("not set")
    };
    key_row.set_title(&t!("{} API key — {}", provider_name(provider), state));
}

/// Whitelist editing group: current packages (removal) + add field + suggestions
/// (installed AUR packages not yet whitelisted).
fn build_whitelist_group(cfg: &Rc<RefCell<Config>>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(t!("Whitelist"))
        .description(t!("Trusted packages: delay skipped, but scan + AI kept"))
        .build();

    let wl_expander = adw::ExpanderRow::builder()
        .title(t!("Whitelist"))
        .subtitle(t!("{} packages", cfg.borrow().whitelist.len()))
        .build();
    let wl_add = adw::EntryRow::builder()
        .title(t!("Add a package…"))
        .show_apply_button(true)
        .build();
    {
        let cfg = cfg.clone();
        let expander = wl_expander.clone();
        wl_add.connect_apply(move |entry| {
            let name = entry.text().trim().to_string();
            if add_to_whitelist(&cfg, &name) {
                expander.add_row(&make_pkg_row(&name, &cfg, &expander));
                update_wl_subtitle(&expander, &cfg);
            }
            entry.set_text("");
        });
    }
    wl_expander.add_row(&wl_add);
    for pkg in cfg.borrow().whitelist.clone() {
        wl_expander.add_row(&make_pkg_row(&pkg, cfg, &wl_expander));
    }
    group.add(&wl_expander);

    // Suggestions: installed AUR packages not in the whitelist.
    let suggestions: Vec<String> = aur::installed_aur_packages()
        .into_iter()
        .filter(|p| !cfg.borrow().is_whitelisted(p))
        .collect();
    if !suggestions.is_empty() {
        let sug_expander = adw::ExpanderRow::builder()
            .title(t!("Suggestions"))
            .subtitle(t!(
                "{} installed AUR packages to whitelist",
                suggestions.len()
            ))
            .build();
        for pkg in suggestions {
            sug_expander.add_row(&make_suggestion_row(&pkg, cfg, &wl_expander, &sug_expander));
        }
        group.add(&sug_expander);
    }

    group
}

// =====================================================================
// Widget helpers
// =====================================================================

/// Adds a package to the whitelist if new. Returns true if added.
fn add_to_whitelist(cfg: &Rc<RefCell<Config>>, name: &str) -> bool {
    if name.is_empty() || cfg.borrow().is_whitelisted(name) {
        return false;
    }
    let mut c = cfg.borrow_mut();
    c.whitelist.push(name.to_string());
    c.whitelist.sort();
    true
}

/// Whitelisted package row with a removal button.
fn make_pkg_row(
    name: &str,
    cfg: &Rc<RefCell<Config>>,
    expander: &adw::ExpanderRow,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(name).build();
    let btn = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .tooltip_text(t!("Remove from whitelist"))
        .build();
    let name_owned = name.to_string();
    let cfg = cfg.clone();
    let expander = expander.clone();
    let row_clone = row.clone();
    btn.connect_clicked(move |_| {
        cfg.borrow_mut().whitelist.retain(|p| p != &name_owned);
        expander.remove(&row_clone);
        update_wl_subtitle(&expander, &cfg);
    });
    row.add_suffix(&btn);
    row
}

/// Suggestion row: a "+" button adds it to the whitelist and moves it.
fn make_suggestion_row(
    name: &str,
    cfg: &Rc<RefCell<Config>>,
    wl_expander: &adw::ExpanderRow,
    sug_expander: &adw::ExpanderRow,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(name).build();
    let btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .tooltip_text(t!("Add to whitelist"))
        .build();
    let name_owned = name.to_string();
    let cfg = cfg.clone();
    let wl_expander = wl_expander.clone();
    let sug_expander = sug_expander.clone();
    let row_clone = row.clone();
    btn.connect_clicked(move |_| {
        if add_to_whitelist(&cfg, &name_owned) {
            wl_expander.add_row(&make_pkg_row(&name_owned, &cfg, &wl_expander));
            update_wl_subtitle(&wl_expander, &cfg);
        }
        sug_expander.remove(&row_clone);
    });
    row.add_suffix(&btn);
    row
}

fn update_wl_subtitle(expander: &adw::ExpanderRow, cfg: &Rc<RefCell<Config>>) {
    expander.set_subtitle(&t!("{} packages", cfg.borrow().whitelist.len()));
}

fn clear_box(b: &gtk::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

// =====================================================================
// Dashboard: summary donut
// =====================================================================

/// A ring segment: a colored verdict count with a short status label.
struct RingSeg {
    count: usize,
    color: Rgb,
    label: String,
}

/// The verdict donut (to install / on hold / blocked) with the total at its
/// center and a legend that focuses a segment on hover. Returned on its own so
/// the persistent hero card can swap it in on every check.
fn summary_ring(summary: &pipeline::Summary) -> gtk::Box {
    let segs: Rc<Vec<RingSeg>> = Rc::new(
        [
            (summary.allowed, COLOR_ALLOW, t!("To install")),
            (summary.delayed, COLOR_DELAY, t!("On hold")),
            (summary.blocked, COLOR_BLOCK, t!("Blocked")),
        ]
        .into_iter()
        .filter(|(count, _, _)| *count > 0)
        .map(|(count, color, label)| RingSeg {
            count,
            color,
            label,
        })
        .collect(),
    );
    let total: usize = segs.iter().map(|s| s.count).sum();

    // Which segment (if any) the pointer is focusing via its legend entry.
    let focus: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));

    // Donut, custom-drawn; the total/label sit in an overlay at its center.
    let area = gtk::DrawingArea::builder()
        .width_request(RING_SIZE)
        .height_request(RING_SIZE)
        .build();
    {
        let segs = segs.clone();
        let focus = focus.clone();
        area.set_draw_func(move |_, cr, w, h| draw_ring(cr, w, h, &segs, focus.get()));
    }

    let center_num = gtk::Label::builder()
        .label(total.to_string())
        .css_classes(["title-1"])
        .build();
    let center_lbl = gtk::Label::builder()
        .label(t!("AUR packages"))
        .css_classes(["dim-label", "caption"])
        .build();
    let center = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .build();
    center.append(&center_num);
    center.append(&center_lbl);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&area));
    overlay.add_overlay(&center);

    // Rewrites the center to the focused segment's count, or the total at rest.
    let refresh_center: Rc<dyn Fn()> = {
        let segs = segs.clone();
        let focus = focus.clone();
        let num = center_num.clone();
        let lbl = center_lbl.clone();
        Rc::new(move || match focus.get() {
            Some(i) => {
                num.set_label(&segs[i].count.to_string());
                lbl.set_label(&segs[i].label);
            }
            None => {
                num.set_label(&total.to_string());
                lbl.set_label(&t!("AUR packages"));
            }
        })
    };

    let legend = gtk::Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    for (i, seg) in segs.iter().enumerate() {
        legend.append(&legend_item(i, seg, &focus, &area, &refresh_center));
    }

    let ring_col = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .valign(gtk::Align::Center)
        .build();
    ring_col.append(&overlay);
    ring_col.append(&legend);
    ring_col
}

/// One-sentence recap of the verdicts, with the official (out-of-scope) count.
/// The three counts are colored to echo the donut segments (blue / orange /
/// grey); the color literals live in the msgid so translators keep them.
fn recap_text(summary: &pipeline::Summary, official: usize) -> String {
    let mut s = t!(
        "<span foreground='#2f79db'><b>{}</b> ready</span> to install, <span foreground='#cf8021'><b>{}</b> maturing</span> under the delay, <span foreground='#8a8784'><b>{}</b> blocked</span>.",
        summary.allowed,
        summary.delayed,
        summary.blocked
    );
    if official > 0 {
        s.push(' ');
        s.push_str(&t!(
            "{} signed official packages are out of scope.",
            official
        ));
    }
    s
}

/// Draws the verdict donut: a faint full-circle track, then one rounded arc per
/// segment. The focused segment thickens; the others dim.
fn draw_ring(cr: &gtk::cairo::Context, w: i32, h: i32, segs: &[RingSeg], focus: Option<usize>) {
    let total: usize = segs.iter().map(|s| s.count).sum();
    if total == 0 {
        return;
    }
    let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
    // Leave room for the thickest (focused) stroke so it never clips the edge.
    let radius = cx.min(cy) - RING_FOCUS_WIDTH / 2.0 - 1.0;

    // Theme-aware track: a faint foreground tint (light on dark, dark on light).
    let fg = if adw::StyleManager::default().is_dark() {
        1.0
    } else {
        0.0
    };
    cr.set_source_rgba(fg, fg, fg, 0.10);
    cr.set_line_width(RING_WIDTH);
    cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
    let _ = cr.stroke();

    cr.set_line_cap(gtk::cairo::LineCap::Round);
    let mut angle = -std::f64::consts::FRAC_PI_2; // start at 12 o'clock
    for (i, seg) in segs.iter().enumerate() {
        let sweep = std::f64::consts::TAU * seg.count as f64 / total as f64;
        let (width, alpha) = match focus {
            Some(f) if f == i => (RING_FOCUS_WIDTH, 1.0),
            Some(_) => (RING_WIDTH, RING_DIM),
            None => (RING_WIDTH, 1.0),
        };
        cr.set_source_rgba(seg.color.0, seg.color.1, seg.color.2, alpha);
        cr.set_line_width(width);
        // Inset each end by half a gap so adjacent arcs read as distinct.
        let half_gap = (RING_GAP / 2.0).min(sweep / 2.0);
        cr.arc(cx, cy, radius, angle + half_gap, angle + sweep - half_gap);
        let _ = cr.stroke();
        angle += sweep;
    }
}

/// A legend entry (colored dot + "count label"). Hovering it focuses the
/// matching ring segment and swaps the donut's center readout.
fn legend_item(
    index: usize,
    seg: &RingSeg,
    focus: &Rc<Cell<Option<usize>>>,
    area: &gtk::DrawingArea,
    refresh_center: &Rc<dyn Fn()>,
) -> gtk::Box {
    let item = gtk::Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();
    item.set_cursor_from_name(Some("pointer"));

    let dot = gtk::DrawingArea::builder()
        .width_request(DOT_SIZE)
        .height_request(DOT_SIZE)
        .valign(gtk::Align::Center)
        .build();
    let color = seg.color;
    dot.set_draw_func(move |_, cr, w, h| {
        let r = w.min(h) as f64 / 2.0;
        cr.set_source_rgb(color.0, color.1, color.2);
        cr.arc(r, r, r, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
    });

    let label = gtk::Label::builder()
        .label(format!("{} {}", seg.count, seg.label.to_lowercase()))
        .css_classes(["caption", "dim-label"])
        .build();
    item.append(&dot);
    item.append(&label);

    // Hover focuses this segment; leaving restores the total.
    let motion = gtk::EventControllerMotion::new();
    {
        let focus = focus.clone();
        let area = area.clone();
        let refresh_center = refresh_center.clone();
        motion.connect_enter(move |_, _, _| {
            focus.set(Some(index));
            refresh_center();
            area.queue_draw();
        });
    }
    {
        let focus = focus.clone();
        let area = area.clone();
        let refresh_center = refresh_center.clone();
        motion.connect_leave(move |_| {
            focus.set(None);
            refresh_center();
            area.queue_draw();
        });
    }
    item.add_controller(motion);
    item
}

/// The app's brand mark for the header: the blue shield + white check from
/// `data/fr.xhelliom.AurVeto.svg`, drawn with cairo so no external icon asset
/// needs to be resolved at runtime. Coordinates are the SVG's 128px viewBox,
/// scaled down to `LOGO_SIZE`.
fn brand_logo() -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .width_request(LOGO_SIZE)
        .height_request(LOGO_SIZE)
        .valign(gtk::Align::Center)
        .build();
    area.set_draw_func(|_, cr, w, h| {
        let scale = w.min(h) as f64 / 128.0;
        cr.scale(scale, scale);

        // Shield outline (vertical gradient fill, dark blue stroke).
        cr.move_to(64.0, 12.0);
        cr.line_to(106.0, 26.0);
        cr.line_to(106.0, 62.0);
        cr.curve_to(106.0, 92.0, 88.0, 110.0, 64.0, 118.0);
        cr.curve_to(40.0, 110.0, 22.0, 92.0, 22.0, 62.0);
        cr.line_to(22.0, 26.0);
        cr.close_path();
        let grad = gtk::cairo::LinearGradient::new(0.0, 12.0, 0.0, 118.0);
        grad.add_color_stop_rgb(0.0, 0.110, 0.443, 0.847); // #1c71d8
        grad.add_color_stop_rgb(1.0, 0.102, 0.373, 0.706); // #1a5fb4
        let _ = cr.set_source(&grad);
        let _ = cr.fill_preserve();
        cr.set_source_rgb(0.051, 0.231, 0.451); // #0d3b73
        cr.set_line_width(3.0);
        let _ = cr.stroke();

        // Validation check.
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        cr.move_to(44.0, 64.0);
        cr.line_to(58.0, 78.0);
        cr.line_to(86.0, 48.0);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.set_line_width(10.0);
        let _ = cr.stroke();
    });
    area
}

// =====================================================================
// Categorized lists
// =====================================================================

/// A left-aligned label carrying a single CSS class.
fn styled(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::builder().label(text).xalign(0.0).build();
    l.add_css_class(class);
    l
}

/// A colored status pill (green "safe" / red "blocked").
fn pill(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class(class);
    l.set_valign(gtk::Align::Center);
    l
}

/// A small colored status dot (`class` = `ag-dot-orange`/`-grey`/…), sized by
/// CSS and vertically centered in its row.
fn status_dot(class: &str) -> gtk::Box {
    let d = gtk::Box::new(Orientation::Horizontal, 0);
    d.add_css_class("ag-dot");
    d.add_css_class(class);
    d.set_halign(gtk::Align::Center);
    d.set_valign(gtk::Align::Center);
    d
}

/// A collapsible section: an uppercase toggle header ("TITLE · N" with a ▼/▶
/// caret and an optional grey suffix) over a content box. Returns
/// `(outer, content)`; the caller fills `content`. Clicking the header toggles
/// the content's visibility.
fn section(title: &str, count: usize, suffix: &str, expanded: bool) -> (gtk::Box, gtk::Box) {
    let content = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .visible(expanded)
        .build();

    let caret = gtk::Label::new(Some(if expanded { "▼" } else { "▶" }));
    caret.add_css_class("ag-caret");
    let label = gtk::Label::new(Some(&format!("{} · {}", title.to_uppercase(), count)));
    label.add_css_class("ag-section-label");

    let head = gtk::Box::new(Orientation::Horizontal, 8);
    head.append(&caret);
    head.append(&label);
    if !suffix.is_empty() {
        let s = gtk::Label::new(Some(suffix));
        s.add_css_class("ag-section-suffix");
        s.set_valign(gtk::Align::Center);
        head.append(&s);
    }

    let header = gtk::Button::builder()
        .css_classes(["ag-section"])
        .halign(gtk::Align::Start)
        .child(&head)
        .build();
    {
        let content = content.clone();
        let caret = caret.clone();
        header.connect_clicked(move |_| {
            let v = !content.is_visible();
            content.set_visible(v);
            caret.set_label(if v { "▼" } else { "▶" });
        });
    }

    let outer = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .build();
    outer.append(&header);
    outer.append(&content);
    (outer, content)
}

/// One "to install" package as a card: the header carries name / version arrow /
/// details, a green "safe" pill and a dark Install button; clicking the text
/// reveals the decision chain (whitelist / anti-revert / scan / AI) that
/// cleared it. `expanded` opens the chain immediately.
fn package_card(o: &Outcome, overlay: &adw::ToastOverlay, expanded: bool) -> gtk::Box {
    let name = styled(&o.update.name, "ag-pkg-name");
    let ver = styled(
        &format!("{} → {}", o.update.old_ver, allow_target(o)),
        "ag-pkg-ver",
    );
    let text = gtk::Box::new(Orientation::Vertical, 3);
    text.set_halign(gtk::Align::Start);
    text.append(&name);
    text.append(&ver);
    let detail = allow_detail(o);
    if !detail.is_empty() {
        text.append(&styled(&detail, "ag-pkg-sub"));
    }

    // Decision chain, revealed / hidden by clicking the package text.
    let chain = gtk::Box::new(Orientation::Vertical, 9);
    chain.add_css_class("ag-chain");
    chain.set_visible(expanded);
    for step in &o.steps {
        chain.append(&chain_step_row(step));
    }

    // Leading caret advertising (and reflecting) the expandable chain.
    let caret = gtk::Label::new(Some(if expanded { "▼" } else { "▶" }));
    caret.add_css_class("ag-caret");
    caret.set_valign(gtk::Align::Center);
    let tog_content = gtk::Box::new(Orientation::Horizontal, 9);
    tog_content.append(&caret);
    tog_content.append(&text);

    let toggler = gtk::Button::builder()
        .css_classes(["ag-toggler"])
        .hexpand(true)
        .halign(gtk::Align::Fill)
        .child(&tog_content)
        .build();
    {
        let chain = chain.clone();
        let caret = caret.clone();
        toggler.connect_clicked(move |_| {
            let v = !chain.is_visible();
            chain.set_visible(v);
            caret.set_label(if v { "▼" } else { "▶" });
        });
    }

    let install = gtk::Button::builder()
        .label(t!("Install"))
        .css_classes(["ag-install"])
        .valign(gtk::Align::Center)
        .build();
    {
        let name = o.update.name.clone();
        let overlay = overlay.clone();
        install.connect_clicked(move |_| install_one(&name, &overlay));
    }

    let head = gtk::Box::new(Orientation::Horizontal, 12);
    head.append(&toggler);
    head.append(&pill(&t!("✓ safe"), "ag-pill-safe"));
    head.append(&install);

    let cardbox = gtk::Box::new(Orientation::Vertical, 0);
    cardbox.add_css_class("ag-card");
    cardbox.append(&head);
    cardbox.append(&chain);
    cardbox
}

/// A blocked package as a card: name, the block reason, a red "blocked" pill,
/// and the (failed) decision chain shown below.
fn blocked_card(o: &Outcome) -> gtk::Box {
    let reason = match &o.decision {
        Decision::Blocked(r) => r.clone(),
        _ => String::new(),
    };
    let name = styled(&o.update.name, "ag-pkg-name");
    let sub = styled(&reason, "ag-step-note");
    sub.set_wrap(true);
    let text = gtk::Box::new(Orientation::Vertical, 3);
    text.set_halign(gtk::Align::Start);
    text.set_hexpand(true);
    text.append(&name);
    text.append(&sub);

    let head = gtk::Box::new(Orientation::Horizontal, 12);
    head.append(&text);
    head.append(&pill(&t!("blocked"), "ag-pill-blocked"));

    let chain = gtk::Box::new(Orientation::Vertical, 9);
    chain.add_css_class("ag-chain");
    for step in &o.steps {
        chain.append(&chain_step_row(step));
    }

    let cardbox = gtk::Box::new(Orientation::Vertical, 0);
    cardbox.add_css_class("ag-card");
    cardbox.append(&head);
    if !o.steps.is_empty() {
        cardbox.append(&chain);
    }
    cardbox
}

/// One decision-chain link: a green (or red/grey) bulleted icon, the step name,
/// and the pipeline's own explanation (never re-derived in the frontend).
fn chain_step_row(step: &ChainStep) -> gtk::Box {
    let row = gtk::Box::new(Orientation::Horizontal, 10);
    row.append(&chain_bullet(step.status));
    row.append(&styled(&step.name, "ag-step-name"));
    if !step.note.is_empty() {
        let note = styled(&format!("— {}", step.note), "ag-step-note");
        note.set_wrap(true);
        row.append(&note);
    }
    row
}

/// A decision-chain step bullet, drawn with cairo: a tinted disc plus a mark
/// (check / cross / dash) whose geometry is centered on the disc — unlike a
/// text glyph, whose font metrics push it off-center inside the circle.
fn chain_bullet(status: StepStatus) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .width_request(BULLET_SIZE)
        .height_request(BULLET_SIZE)
        .valign(gtk::Align::Center)
        .build();
    area.set_draw_func(move |_, cr, w, h| {
        let n = w.min(h) as f64;
        let c = n / 2.0;

        // Tinted disc + the mark's stroke color, per status.
        let (fill, stroke): (Rgba, Rgb) = match status {
            StepStatus::Passed => ((0.227, 0.639, 0.361, 0.16), (0.184, 0.502, 0.286)),
            StepStatus::Failed => ((0.784, 0.192, 0.184, 0.15), (0.690, 0.165, 0.157)),
            StepStatus::Skipped => ((0.078, 0.078, 0.071, 0.08), (0.078, 0.078, 0.071)),
        };
        cr.arc(c, c, c, 0.0, std::f64::consts::TAU);
        cr.set_source_rgba(fill.0, fill.1, fill.2, fill.3);
        let _ = cr.fill();

        cr.set_line_width(n * 0.095);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        match status {
            StepStatus::Passed => {
                cr.set_source_rgb(stroke.0, stroke.1, stroke.2);
                cr.move_to(n * 0.30, n * 0.52);
                cr.line_to(n * 0.44, n * 0.66);
                cr.line_to(n * 0.72, n * 0.35);
            }
            StepStatus::Failed => {
                cr.set_source_rgb(stroke.0, stroke.1, stroke.2);
                cr.move_to(n * 0.35, n * 0.35);
                cr.line_to(n * 0.65, n * 0.65);
                cr.move_to(n * 0.65, n * 0.35);
                cr.line_to(n * 0.35, n * 0.65);
            }
            StepStatus::Skipped => {
                // A muted dash: the step did not run.
                cr.set_source_rgba(stroke.0, stroke.1, stroke.2, 0.55);
                cr.move_to(n * 0.32, c);
                cr.line_to(n * 0.68, c);
            }
        }
        let _ = cr.stroke();
    });
    area
}

/// The "on hold" list: a grouped card of pending packages (orange dot, name,
/// latest published version, and the countdown). Only the first
/// `VISIBLE_WAITING` show; the rest fold behind a "+N more" reveal.
fn waiting_list(delayed: &[&Outcome], now: u64) -> gtk::Box {
    let list = gtk::Box::new(Orientation::Vertical, 1);
    list.add_css_class("ag-list");

    let mut hidden: Vec<gtk::Box> = Vec::new();
    for (i, o) in delayed.iter().enumerate() {
        let row = waiting_row(o, now);
        if i >= VISIBLE_WAITING {
            row.set_visible(false);
            hidden.push(row.clone());
        }
        list.append(&row);
    }

    if !hidden.is_empty() {
        let more = gtk::Button::builder()
            .label(t!("+ {} more maturing packages", hidden.len()))
            .css_classes(["ag-more"])
            .build();
        more.connect_clicked(move |b| {
            for r in &hidden {
                r.set_visible(true);
            }
            b.set_visible(false);
        });
        list.append(&more);
    }
    list
}

/// One pending-package row for the "on hold" list.
fn waiting_row(o: &Outcome, now: u64) -> gtk::Box {
    let row = gtk::Box::new(Orientation::Horizontal, 12);
    row.add_css_class("ag-row");
    row.append(&status_dot("ag-dot-orange"));

    let text = gtk::Box::new(Orientation::Vertical, 1);
    text.set_hexpand(true);
    text.set_halign(gtk::Align::Start);
    text.append(&styled(&o.update.name, "ag-row-title"));
    if !o.update.new_ver.is_empty() {
        text.append(&styled(
            &t!("latest published: {}", o.update.new_ver),
            "ag-row-sub",
        ));
    }
    row.append(&text);

    let meta = styled(&waiting_days(o, now), "ag-row-meta");
    meta.set_valign(gtk::Align::Center);
    row.append(&meta);
    row
}

/// Countdown label of a pending package ("~N d left", or "on hold" if the date
/// is unknown).
fn waiting_days(o: &Outcome, now: u64) -> String {
    match o.eligible_at {
        Some(ts) if ts > now => t!(
            "{}d left",
            ts.saturating_sub(now).div_ceil(aur::SECS_PER_DAY)
        ),
        _ => t!("on hold"),
    }
}

/// The "official repositories" list: grey-dotted rows (name, version, the
/// `pacman -Syu` that installs them). Each `line` comes from `checkupdates`
/// ("name oldver -> newver"), from which we surface the name and target version.
fn official_list(lines: &[String]) -> gtk::Box {
    let list = gtk::Box::new(Orientation::Vertical, 1);
    list.add_css_class("ag-list");
    for line in lines {
        let mut parts = line.split_whitespace();
        let name = parts.next().unwrap_or(line);
        let ver = line.split_whitespace().last().unwrap_or("");

        let row = gtk::Box::new(Orientation::Horizontal, 12);
        row.add_css_class("ag-row");
        row.append(&status_dot("ag-dot-grey"));
        let name_lbl = styled(name, "ag-row-title");
        name_lbl.set_hexpand(true);
        row.append(&name_lbl);
        let ver_lbl = styled(ver, "ag-pkg-ver");
        ver_lbl.set_valign(gtk::Align::Center);
        row.append(&ver_lbl);
        let cmd = styled("pacman -Syu", "ag-row-meta");
        cmd.set_valign(gtk::Align::Center);
        row.append(&cmd);
        list.append(&row);
    }
    list
}

// =====================================================================
// Transient / empty states
// =====================================================================

fn info_row(text: &str) -> adw::ActionRow {
    adw::ActionRow::builder().title(text).build()
}

/// A spinning placeholder shown in the results area while a check runs, so
/// the page never looks simply frozen/blank.
fn loading_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(t!("Checking your AUR updates…"))
        .build();
    row.add_prefix(&gtk::Spinner::builder().spinning(true).build());
    row
}

/// Wraps a single row/expander in its own rounded "card" (a one-item
/// boxed-list); used for the transient / empty states (loading, error,
/// "everything up to date").
fn card(child: &impl IsA<gtk::Widget>) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(child);
    list
}

/// Registers the theme stylesheet for the whole display (once).
fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(THEME_CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// "Nothing to do" row: confirms everything is up to date AND, in the subtitle,
/// recalls the maturation policy — so a new user understands that future updates
/// will be offered later, not that they are absent.
fn up_to_date_row(cfg: &Config) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(t!("Everything is up to date."))
        .build();
    if cfg.delay_days > 0 {
        let policy = match cfg.delay_mode {
            DelayMode::Lag => t!(
                "New AUR updates will be offered after {}d of maturation (lag mode).",
                cfg.delay_days
            ),
            DelayMode::Hold => t!(
                "New AUR updates are held for {}d before being offered (hold mode).",
                cfg.delay_days
            ),
        };
        row.set_subtitle(&policy);
        row.set_subtitle_lines(0); // no truncation: the reminder must be read in full
    }
    row.add_prefix(&gtk::Image::from_icon_name("emblem-ok-symbolic"));
    row
}

// =====================================================================
// Verdict data helpers (formatting only — no decisions taken here)
// =====================================================================

/// Label for the age of the targeted deferred revision (target commit date).
/// `committed_at == 0` means the date is unreadable: we say so rather than lie.
fn lag_age_label(target: &aur::LagTarget) -> String {
    if target.committed_at == 0 {
        return t!("revision age unknown");
    }
    let age = aur::now_secs().saturating_sub(target.committed_at) / aur::SECS_PER_DAY;
    t!("revision {}d old", age)
}

/// Label of the latest published version and its age.
fn latest_label(o: &Outcome) -> String {
    match o.age_days {
        Some(w) => t!("latest {} published {}d ago", o.update.new_ver, w),
        None => t!("latest {}", o.update.new_ver),
    }
}

/// Target version of an allowed package: the installed lag revision, otherwise the latest.
fn allow_target(o: &Outcome) -> String {
    match &o.lag {
        Some(t) => t.version.clone(),
        None if !o.update.new_ver.is_empty() => o.update.new_ver.clone(),
        None => o.update.old_ver.clone(),
    }
}

/// Secondary details of an allowed package: current version, age of the lag
/// revision, and latest published version if it differs from the installed one.
fn allow_detail(o: &Outcome) -> String {
    let mut parts = Vec::new();
    if !o.update.old_ver.is_empty() {
        parts.push(t!("current {}", o.update.old_ver));
    }
    if let Some(target) = &o.lag {
        parts.push(lag_age_label(target));
        if !o.update.new_ver.is_empty() && o.update.new_ver != target.version {
            parts.push(latest_label(o));
        }
    }
    if o.whitelisted {
        parts.push(t!("whitelisted"));
    }
    parts.join("  ·  ")
}

/// Wraps a string in single quotes to inject it safely into a `bash -c` line
/// (paths containing spaces). Internal single quotes are escaped via the `'\''`
/// sequence.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Tries to launch a command in a common terminal emulator.
fn launch_in_terminal(cmd: &str) -> std::io::Result<()> {
    let full = format!("{cmd}; echo; read -p '{}'", t!("Press Enter to close…"));
    let candidates: [(&str, Vec<&str>); 4] = [
        ("foot", vec!["-e", "bash", "-c", &full]),
        ("kitty", vec!["bash", "-c", &full]),
        ("alacritty", vec!["-e", "bash", "-c", &full]),
        ("xterm", vec!["-e", "bash", "-c", &full]),
    ];
    for (term, args) in candidates {
        if std::process::Command::new(term).args(&args).spawn().is_ok() {
            return Ok(());
        }
    }
    Ok(())
}
