//! GTK4 / libadwaita graphical interface for aurveto.
//!
//! Main view: the AUR updates (check + verdicts + apply).
//! The settings live in a separate dialog (gear button).

use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{glib, Adjustment, Orientation, StringList};
use libadwaita as adw;
use libadwaita::prelude::*;

use aurveto::config::{Config, DelayMode, Provider, Secrets};
use aurveto::pipeline::{self, Decision, Outcome};
use aurveto::{aur, deploy, t};

const APP_ID: &str = "fr.xhelliom.AurVeto";

/// RGB color (0..1) of a distribution-bar segment / a swatch.
type Rgb = (f64, f64, f64);

/// Height of the distribution bar (px).
const BAR_HEIGHT: i32 = 18;
/// Side of a legend swatch (px).
const SWATCH_SIZE: i32 = 12;

// Category palette: aligned with Adwaita's semantic colors
// (accent/green/blue/orange/red) to stay readable in light and dark themes.
const COLOR_OFFICIAL: Rgb = (0.38, 0.49, 0.55); // slate — signed repos
const COLOR_ALLOW: Rgb = (0.18, 0.76, 0.49); // green — installed at latest version
const COLOR_LAG: Rgb = (0.20, 0.56, 0.85); // blue — installed at deferred revision
const COLOR_DELAY: Rgb = (0.96, 0.55, 0.06); // orange — delayed
const COLOR_BLOCK: Rgb = (0.88, 0.11, 0.14); // red — blocked

/// Status badge styles: colored pills. We rely on libadwaita's named colors
/// (`@success_color`…) to stay readable in light and dark themes, without
/// hard-coding any hue.
const BADGE_CSS: &str = "\
.ag-badge { border-radius: 12px; padding: 1px 9px; font-weight: bold; }
.ag-badge.ag-ok   { background-color: alpha(@success_color, 0.15); color: @success_color; }
.ag-badge.ag-warn { background-color: alpha(@warning_color, 0.15); color: @warning_color; }
.ag-badge.ag-err  { background-color: alpha(@error_color, 0.15); color: @error_color; }
";

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
    install_css();
    let cfg = Rc::new(RefCell::new(Config::load_or_init().unwrap_or_default()));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("aurveto")
        .default_width(560)
        .default_height(720)
        .build();

    let header = adw::HeaderBar::new();
    let settings_btn = gtk::Button::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text(t!("Settings"))
        .build();
    header.pack_end(&settings_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);

    let page = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(18)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();

    let updates = adw::PreferencesGroup::builder()
        .title(t!("AUR updates"))
        .description(t!("Checks packages against the configured decision chain"))
        .build();

    let check_btn = gtk::Button::builder()
        .label(t!("Check"))
        .css_classes(["pill"])
        .build();
    let apply_btn = gtk::Button::builder()
        .label(t!("Update selection"))
        .css_classes(["pill"])
        .tooltip_text(t!("Install only the checked AUR packages"))
        .sensitive(false)
        .build();
    let upgrade_btn = gtk::Button::builder()
        .label(t!("Update everything"))
        .css_classes(["suggested-action", "pill"])
        .tooltip_text(t!("Official repos (pacman -Syu) then safe AUR packages"))
        .build();
    let btn_box = gtk::Box::new(Orientation::Horizontal, 8);
    btn_box.append(&check_btn);
    btn_box.append(&apply_btn);
    btn_box.append(&upgrade_btn);
    updates.set_header_suffix(Some(&btn_box));

    // Checkboxes for the "allowed" packages (name, widget): filled by the
    // check, read by the selective update.
    let selected: Rc<RefCell<Vec<(String, gtk::CheckButton)>>> = Rc::new(RefCell::new(Vec::new()));

    let results = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    results.append(&info_row(&t!("Click “Check” to run the analysis.")));
    updates.add(&results);

    // Dashboard (KPIs + distribution bar), filled by the check.
    let dashboard = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .build();

    page.append(&dashboard);
    page.append(&updates);

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

    wire_check(
        &cfg, &check_btn, &dashboard, &results, &selected, &apply_btn,
    );
    wire_apply(&apply_btn, &selected, &overlay);
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
    dashboard: &gtk::Box,
    results: &gtk::ListBox,
    selected: &Rc<RefCell<Vec<(String, gtk::CheckButton)>>>,
    apply_btn: &gtk::Button,
) {
    let cfg = cfg.clone();
    let dashboard = dashboard.clone();
    let results = results.clone();
    let selected = selected.clone();
    let apply_btn = apply_btn.clone();
    let check_btn_outer = check_btn.clone();
    check_btn.connect_clicked(move |_| {
        check_btn_outer.set_sensitive(false);
        check_btn_outer.set_label(&t!("Checking…"));
        clear_box(&dashboard);
        clear_listbox(&results);
        selected.borrow_mut().clear();
        apply_btn.set_sensitive(false);

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
        let dashboard = dashboard.clone();
        let results = results.clone();
        let selected = selected.clone();
        let apply_btn = apply_btn.clone();
        let check_btn_inner = check_btn_outer.clone();
        glib::spawn_future_local(async move {
            if let Ok(res) = rx.recv().await {
                match res {
                    Ok((official, outcomes)) => {
                        render(
                            &cfg.borrow(),
                            &dashboard,
                            &results,
                            &official,
                            &outcomes,
                            &selected,
                            &apply_btn,
                        );
                    }
                    Err(e) => results.append(&info_row(&t!("Error: {}", e))),
                }
            }
            check_btn_inner.set_sensitive(true);
            check_btn_inner.set_label(&t!("Check"));
        });
    });
}

/// Populates the dashboard (KPIs + bar) and the collapsible lists from the
/// verdicts. All the decision-making is already done by `pipeline`; we only
/// present and group.
fn render(
    cfg: &Config,
    dashboard: &gtk::Box,
    results: &gtk::ListBox,
    official: &[String],
    outcomes: &[Outcome],
    selected: &Rc<RefCell<Vec<(String, gtk::CheckButton)>>>,
    apply_btn: &gtk::Button,
) {
    clear_box(dashboard);
    clear_listbox(results);
    selected.borrow_mut().clear();

    if official.is_empty() && outcomes.is_empty() {
        results.append(&up_to_date_row(cfg));
        apply_btn.set_sensitive(false);
        return;
    }

    let summary = pipeline::summarize(outcomes);
    dashboard.append(&kpi_row(official.len(), &summary));
    dashboard.append(&distribution_bar(official, outcomes, &summary));

    // Blocked first (the most important), expanded.
    let blocked: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| matches!(o.decision, Decision::Blocked(_)))
        .collect();
    if !blocked.is_empty() {
        let exp = group_expander(
            &t!("Blocked"),
            blocked.len(),
            true,
            "dialog-warning-symbolic",
        );
        for o in &blocked {
            exp.add_row(&outcome_row(o));
        }
        results.append(&exp);
    }

    // To install, expanded, with checkboxes (selection = restriction).
    let allowed: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| o.decision == Decision::Allow)
        .collect();
    if !allowed.is_empty() {
        let exp = group_expander(&t!("To install"), allowed.len(), true, "emblem-ok-symbolic");
        for o in &allowed {
            let row = outcome_row(o);
            let check = gtk::CheckButton::builder()
                .tooltip_text(t!("Include in the selective update"))
                .build();
            row.add_suffix(&check);
            selected.borrow_mut().push((o.update.name.clone(), check));
            exp.add_row(&row);
        }
        results.append(&exp);
    }

    // Delayed and official repos: collapsed by default (informational).
    let delayed: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| matches!(o.decision, Decision::Delayed(_)))
        .collect();
    if !delayed.is_empty() {
        let exp = group_expander(
            &t!("On hold"),
            delayed.len(),
            false,
            "appointment-soon-symbolic",
        );
        for o in &delayed {
            exp.add_row(&outcome_row(o));
        }
        results.append(&exp);
    }

    if !official.is_empty() {
        let exp = group_expander(
            &t!("Official repositories (signed)"),
            official.len(),
            false,
            "package-x-generic-symbolic",
        );
        for line in official {
            exp.add_row(&info_row(line));
        }
        results.append(&exp);
    }

    apply_btn.set_sensitive(!selected.borrow().is_empty());
}

/// Wires the "Update selection" button: runs `aurveto apply` (AUR packages
/// only, without touching the official repos) restricted to the checked
/// packages. The CLI re-evaluates the decision chain at install time: the
/// selection bypasses no guard, it only narrows.
fn wire_apply(
    apply_btn: &gtk::Button,
    selected: &Rc<RefCell<Vec<(String, gtk::CheckButton)>>>,
    overlay: &adw::ToastOverlay,
) {
    let selected = selected.clone();
    let overlay = overlay.clone();
    apply_btn.connect_clicked(move |_| {
        let names: Vec<String> = selected
            .borrow()
            .iter()
            .filter(|(_, check)| check.is_active())
            .map(|(name, _)| name.clone())
            .collect();
        if names.is_empty() {
            overlay.add_toast(adw::Toast::new(&t!("Select at least one package first")));
            return;
        }
        let cli = sh_quote(&deploy::cli_command());
        let args: String = names.iter().map(|n| format!(" {}", sh_quote(n))).collect();
        let _ = launch_in_terminal(&format!("{cli} apply{args}"));
        overlay.add_toast(adw::Toast::new(&t!(
            "Selective update started in a terminal"
        )));
    });
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

fn clear_listbox(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn clear_box(b: &gtk::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

// =====================================================================
// Dashboard: KPIs + distribution bar
// =====================================================================

/// Row of KPI cards summarising what will (or won't) be updated.
fn kpi_row(official: usize, summary: &pipeline::Summary) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .homogeneous(true)
        .build();
    row.append(&kpi_card(official, &t!("Official"), "accent"));
    row.append(&kpi_card(summary.allowed, &t!("To install"), "success"));
    row.append(&kpi_card(summary.delayed, &t!("On hold"), "warning"));
    row.append(&kpi_card(summary.blocked, &t!("Blocked"), "error"));
    row
}

/// A KPI card: large colored number + label. `accent` is an Adwaita semantic
/// style class (accent/success/warning/error).
fn kpi_card(value: usize, label: &str, accent: &str) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .css_classes(["card"])
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(8)
        .margin_end(8)
        .build();
    let num = gtk::Label::builder()
        .label(value.to_string())
        .css_classes(["title-1", accent])
        .build();
    let lbl = gtk::Label::builder()
        .label(label)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label", "caption"])
        .build();
    card.append(&num);
    card.append(&lbl);
    card
}

/// A distribution-bar category: label, count, color.
struct Segment {
    label: String,
    count: usize,
    color: Rgb,
}

/// Horizontal segmented bar (proportional to the counts) + its legend.
/// Visualizes at a glance official / to install / deferred / delayed / blocked.
fn distribution_bar(
    official: &[String],
    outcomes: &[Outcome],
    summary: &pipeline::Summary,
) -> gtk::Box {
    // Within the "allowed", distinguish latest version from deferred revision.
    let lagged = outcomes
        .iter()
        .filter(|o| o.decision == Decision::Allow && o.lag.is_some())
        .count();
    let latest = summary.allowed.saturating_sub(lagged);

    let segments = [
        Segment {
            label: t!("Official"),
            count: official.len(),
            color: COLOR_OFFICIAL,
        },
        Segment {
            label: t!("Latest"),
            count: latest,
            color: COLOR_ALLOW,
        },
        Segment {
            label: t!("Deferred"),
            count: lagged,
            color: COLOR_LAG,
        },
        Segment {
            label: t!("On hold"),
            count: summary.delayed,
            color: COLOR_DELAY,
        },
        Segment {
            label: t!("Blocked"),
            count: summary.blocked,
            color: COLOR_BLOCK,
        },
    ];

    let container = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .build();

    let bar = gtk::DrawingArea::builder()
        .height_request(BAR_HEIGHT)
        .hexpand(true)
        .build();
    bar.add_css_class("card");
    let drawn: Vec<(usize, Rgb)> = segments.iter().map(|s| (s.count, s.color)).collect();
    bar.set_draw_func(move |_, cr, width, height| {
        let total: usize = drawn.iter().map(|(c, _)| c).sum();
        if total == 0 {
            return;
        }
        let (w, h) = (width as f64, height as f64);
        let mut x = 0.0;
        for (i, (count, color)) in drawn.iter().enumerate() {
            // The last segment runs to the edge to absorb rounding errors.
            let seg_w = if i + 1 == drawn.len() {
                w - x
            } else {
                w * *count as f64 / total as f64
            };
            cr.set_source_rgb(color.0, color.1, color.2);
            cr.rectangle(x, 0.0, seg_w, h);
            let _ = cr.fill();
            x += seg_w;
        }
    });
    container.append(&bar);

    // Legend: one swatch per non-empty category.
    let legend = gtk::Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(14)
        .halign(gtk::Align::Center)
        .build();
    for s in segments.iter().filter(|s| s.count > 0) {
        legend.append(&legend_item(s));
    }
    container.append(&legend);

    container
}

/// A legend entry: colored swatch + "label count".
fn legend_item(seg: &Segment) -> gtk::Box {
    let item = gtk::Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();
    let swatch = gtk::DrawingArea::builder()
        .width_request(SWATCH_SIZE)
        .height_request(SWATCH_SIZE)
        .valign(gtk::Align::Center)
        .build();
    let color = seg.color;
    swatch.set_draw_func(move |_, cr, width, height| {
        cr.set_source_rgb(color.0, color.1, color.2);
        cr.rectangle(0.0, 0.0, width as f64, height as f64);
        let _ = cr.fill();
    });
    let label = gtk::Label::builder()
        .label(format!("{} {}", seg.label, seg.count))
        .css_classes(["caption"])
        .build();
    item.append(&swatch);
    item.append(&label);
    item
}

/// Collapsible row grouping packages of the same category (title + counter).
fn group_expander(title: &str, count: usize, expanded: bool, icon: &str) -> adw::ExpanderRow {
    let exp = adw::ExpanderRow::builder()
        .title(title)
        .subtitle(t!("{} packages", count))
        .expanded(expanded)
        .build();
    exp.add_prefix(&gtk::Image::from_icon_name(icon));
    exp
}

fn info_row(text: &str) -> adw::ActionRow {
    adw::ActionRow::builder().title(text).build()
}

/// Registers the badge stylesheet for the whole display (once).
fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(BADGE_CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Colored status pill: `variant` = CSS class (`ag-ok`/`ag-warn`/`ag-err`).
fn badge(text: &str, variant: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .valign(gtk::Align::Center)
        .css_classes(["ag-badge", variant])
        .build()
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

/// Formats a Unix timestamp as a short local date (via glib, no dependency).
fn format_date(ts: u64) -> String {
    glib::DateTime::from_unix_local(ts as i64)
        .and_then(|d| d.format("%x"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

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

/// Version that will actually be installed at the deadline of a pending package.
fn delayed_target(o: &Outcome) -> String {
    o.eligible_version
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if o.update.new_ver.is_empty() {
                o.update.old_ver.clone()
            } else {
                o.update.new_ver.clone()
            }
        })
}

/// Subtitle (secondary details) of an allowed package: current version, age of
/// the lag revision, and latest published version if it differs from the installed one.
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

/// Subtitle (secondary details) of a pending package: availability date,
/// current version, and latest published version if it differs from the target.
fn delayed_detail(o: &Outcome, days_since_mod: u64, now: u64) -> String {
    let mut parts = Vec::new();
    if let Some(ts) = o.eligible_at {
        if ts > now {
            parts.push(t!("available on {}", format_date(ts)));
        }
    }
    if !o.update.old_ver.is_empty() {
        parts.push(t!("current {}", o.update.old_ver));
    }
    let target = delayed_target(o);
    if !o.update.new_ver.is_empty() && o.update.new_ver != target {
        parts.push(t!(
            "latest {} published {}d ago",
            o.update.new_ver,
            days_since_mod
        ));
    }
    parts.join("  ·  ")
}

/// Countdown badge of a pending package ("~N d", orange).
fn delayed_badge(o: &Outcome, now: u64) -> gtk::Label {
    match o.eligible_at {
        Some(ts) if ts > now => {
            let days = ts.saturating_sub(now).div_ceil(aur::SECS_PER_DAY);
            badge(&t!("~{}d", days), "ag-warn")
        }
        _ => badge(&t!("on hold"), "ag-warn"),
    }
}

/// A verdict row: title = "package → target version" (clearly visible), colored
/// status badge on the right, greyed-out details in the subtitle. Badge color +
/// title = "which version, which status" grasped at a glance.
fn outcome_row(o: &Outcome) -> adw::ActionRow {
    let now = aur::now_secs();
    let row = adw::ActionRow::builder().build();
    row.set_use_markup(false); // literal versions/reasons (no escaping required)
    row.set_subtitle_lines(0); // sometimes long details: do not truncate
    let icon = match &o.decision {
        Decision::Allow => {
            row.set_title(&format!("{} → {}", o.update.name, allow_target(o)));
            row.set_subtitle(&allow_detail(o));
            row.add_suffix(&badge(&t!("to install"), "ag-ok"));
            "emblem-ok-symbolic"
        }
        Decision::Delayed(d) => {
            row.set_title(&format!("{} → {}", o.update.name, delayed_target(o)));
            row.set_subtitle(&delayed_detail(o, *d, now));
            row.add_suffix(&delayed_badge(o, now));
            "appointment-soon-symbolic"
        }
        Decision::Blocked(reason) => {
            row.set_title(&o.update.name);
            row.set_subtitle(&t!("BLOCKED — {}", reason));
            row.add_suffix(&badge(&t!("blocked"), "ag-err"));
            "dialog-warning-symbolic"
        }
    };
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
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
