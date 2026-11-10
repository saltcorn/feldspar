//! The admin SPA's theme is **Tabler**, vendored as CSS, and that has two
//! properties the server depends on and neither the Rust nor the TypeScript
//! compiler can check.
//!
//! 1. **It is same-origin-clean.** Every response carries `style-src 'self'`
//!    and `font-src 'self'` (see `security::CONTENT_SECURITY_POLICY`), so a
//!    stylesheet that `@import`s a webfont or references a remote image is not
//!    a cosmetic regression — the browser refuses to load it and the admin UI
//!    renders unstyled. Tabler's own templates *do* `@import` Inter from
//!    `rsms.me`, so this is a live hazard on the next theme upgrade, and this
//!    test is what catches it.
//! 2. **It is what the bundle actually loads.** The stylesheet is only served
//!    because `main.tsx` imports it; an import left pointing at Bootstrap's
//!    plain stylesheet would type-check, build and lose the entire theme.
//!
//! Like the other `ui/admin` tests, this reads the checked-in source rather
//! than a build, so it needs no Node toolchain.
// `allow-expect-in-tests` covers `#[cfg(test)]` modules and not an integration
// test's own body, so every test file in this crate says so for itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

/// The `ui/admin` directory.
fn ui() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/admin")
}

fn read(rel: &str) -> String {
    let path = ui().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn vendored_tabler_css_is_self_contained() {
    let css = read("src/vendor/tabler/tabler.min.css");

    // The upstream banner: proof this is the released build and not a
    // hand-edited copy, and the MIT notice the licence requires be kept.
    assert!(
        css.contains("Tabler v") && css.contains("Licensed under MIT"),
        "the vendored stylesheet should keep Tabler's banner comment (version + MIT notice)"
    );

    // No `@import` at all: every one of them is a second request, and the CSP
    // allows none of them to leave the origin.
    assert!(
        !css.contains("@import"),
        "the vendored stylesheet must not @import anything (CSP: style-src 'self'); \
         Tabler's templates import the Inter webfont — that line must not be copied in"
    );

    // Every `url(...)` must be a `data:` URI. `img-src 'self' data:` permits
    // those and nothing else remote, and a relative path would name an asset
    // directory this repo does not vendor.
    for (index, rest) in css.match_indices("url(").map(|(i, _)| (i, &css[i + 4..])) {
        let value = rest.trim_start_matches(['"', '\'']);
        assert!(
            value.starts_with("data:"),
            "url() at byte {index} is not a data: URI: {}",
            &value[..value.len().min(60)]
        );
    }
}

/// Does any rule in the minified `css` whose selector list includes exactly
/// `selector` carry `declaration`? Written against the minified file because
/// that is what ships: `.page` matches `.page{…}` and `a,.page{…}` but not
/// `.page-wrapper{…}` or `.card .page{…}`, and a rule inside an `@media` block
/// counts like any other.
fn declares(css: &str, selector: &str, declaration: &str) -> bool {
    css.split('}').any(|chunk| {
        let Some((head, body)) = chunk.rsplit_once('{') else {
            return false;
        };
        let selectors = head.rsplit('{').next().unwrap_or(head);
        selectors.split(',').any(|one| one.trim() == selector) && body.contains(declaration)
    })
}

/// The folded sidebar (Tabler's `navbar-folded`): a rail of icons that the
/// admin folds and unfolds with a button, and stays that way until clicked.
///
/// Tabler 1.5 owns the geometry — the rail's width, the page wrapper's matching
/// offset, the collapsed link titles — so there is nothing here to check it
/// against. What is checkable is the seam: the class `App.tsx` writes has to be
/// one the vendored stylesheet actually styles, and the button that undoes the
/// fold must not be one Tabler hides in the folded state.
#[test]
fn the_sidebar_folds_the_way_tabler_folds_it() {
    let app = read("src/App.tsx");
    let css = read("src/admin.css");
    let tabler = read("src/vendor/tabler/tabler.min.css");

    // The switch itself. `navbar-folded` is a Tabler class, so a typo in it is
    // silent — the sidebar simply never folds.
    assert!(
        app.contains(r#"folded ? " navbar-folded" : """#) && app.contains("useFoldedSidebar"),
        "App.tsx should fold the sidebar with Tabler's `navbar-folded`"
    );
    assert!(
        tabler.contains(".navbar-folded,") || tabler.contains(".navbar-folded "),
        "the vendored Tabler stylesheet should define `.navbar-folded`"
    );
    // Not the hover variant: a sidebar that springs open under a passing
    // pointer was tried and rejected.
    // (Named in prose in App.tsx, explaining why it is not used; a use would
    // be the class string.)
    assert!(
        !app.contains(r#"" navbar-folded-hover""#),
        "the sidebar should fold statically, not unfold on hover"
    );

    // Tabler hides `[data-bs-toggle=sidebar-folded]` outright inside a
    // `navbar-folded` sidebar — it is the pin of the hover variant. On the fold
    // switch it would leave a folded sidebar with no way back.
    assert!(
        tabler.contains(
            ".navbar-vertical.navbar-expand-lg:is(.navbar-folded,.navbar-folded-hover:not(:hover):not(:has(:focus-visible))) [data-bs-toggle=sidebar-folded]"
        ),
        "Tabler should still hide its own fold toggle in a folded sidebar"
    );
    assert!(
        !app.contains(r#"data-bs-toggle="sidebar-folded""#),
        "the fold switch must not carry the attribute Tabler hides when folded"
    );
    assert!(
        app.contains("onClick={toggleFolded}") && app.contains("aria-pressed={folded}"),
        "the sidebar should have a button that toggles the fold"
    );
    // Folded, a link's icon is all there is to go on. The hover label is the
    // name in the reader's language, the same text as the (collapsed) title.
    assert!(
        app.contains("const label = navLabel(item.label, t);")
            && app.contains("title={folded ? label : undefined}")
            && app.contains(r#"<span className="nav-link-title">{label}</span>"#),
        "a folded nav link should carry its name as a hover label"
    );

    // `admin.css` must not restate the geometry: a width or an offset written
    // here is a second opinion about one measurement, and the two drift apart on
    // the next upgrade.
    assert!(
        !css.contains("var(--tblr-sidebar-folded-width)") && !css.contains("sidebar-narrow"),
        "admin.css should leave the folded sidebar's geometry entirely to Tabler"
    );

    // The furniture rules that are ours key on the same class.
    for element in [
        "ms-2 sidebar-wide-only",
        "text-truncate mb-2 sidebar-wide-only",
    ] {
        assert!(
            app.contains(element),
            "App.tsx should mark `{element}` as surviving only the unfolded sidebar"
        );
    }
    assert!(
        css.contains(".navbar-folded .sidebar-wide-only") && !css.contains("navbar-folded-hover"),
        "admin.css should hide `.sidebar-wide-only` in Tabler's `navbar-folded` state"
    );

    // The mode only exists inside Tabler's `lg` breakpoint, where the sidebar is
    // a rail; below it the sidebar is a drawer and hiding its prose would leave
    // a menu of unexplained icons.
    assert!(
        css.contains("@media (min-width: 992px)"),
        "the folded-sidebar rules should be scoped to Tabler's `lg` breakpoint"
    );
}

/// Two `admin.css` rules that Tabler 1.5 still does not make redundant, and one
/// it does — each checked against the theme it was compensating for, so this
/// file shrinks on an upgrade rather than accumulating.
#[test]
fn the_gaps_admin_css_fills_are_still_gaps() {
    let css = read("src/admin.css");
    let tabler = read("src/vendor/tabler/tabler.min.css");

    // 1. `.page` is a percentage of its parent's height, which only resolves
    //    against ancestors that have a height of their own. Tabler gives `body`
    //    one; nothing gives `html` or `#root` (the SPA's mount point) one, so
    //    without this rule the chain falls back to `auto` and the sidebar and
    //    the page background stop partway down a short screen.
    assert!(
        declares(&tabler, ".page", "min-height:100%"),
        "Tabler's `.page` should still be sized as a percentage of its parent"
    );
    assert!(
        css.contains("html,\nbody,\n#root {\n  height: 100%;\n}"),
        "admin.css should give the whole chain above `.page` a height"
    );

    // 2. Reboot's `<pre>` scrolls sideways rather than wrapping, which hides the
    //    end of a long build diagnostic.
    assert!(
        declares(&tabler, "pre", "overflow:auto") && !declares(&tabler, "pre", "white-space"),
        "Tabler should still leave `<pre>` scrolling sideways rather than wrapping"
    );
    assert!(
        css.contains(".text-pre-wrap"),
        "admin.css should provide the wrapping `<pre>` the build logs need"
    );

    // And the one Tabler took over: 1.5.1 gave `.btn-icon` a `min-width` off the
    // same formula as `.btn`'s height, so an icon button is square at every size
    // without help. Restating its padding here would now be a second opinion
    // about one measurement.
    assert!(
        declares(&tabler, ".btn-icon", "min-width:calc("),
        "Tabler should still square `.btn-icon` itself"
    );
    assert!(
        !css.contains(".btn-icon"),
        "admin.css should not restate `.btn-icon` sizing — Tabler 1.5.1 fixed it"
    );
}

/// The chat screen (§11.4) is the one admin page that claims the viewport: the
/// transcript scrolls inside it and the composer never moves. That is a
/// contract between two files — a class name in `AgentChat.tsx` and the rules
/// `admin.css` keys on it — and nothing in the build would notice them
/// diverging. A renamed class gives back a page that scrolls as a whole, with
/// the box you type into somewhere below the fold.
#[test]
fn the_chat_screen_and_its_full_height_rules_agree() {
    let css = read("src/admin.css");
    let screen = read("src/screens/AgentChat.tsx");

    for class in [
        // The screen's own root, which the wrapper's rules select on.
        "chat-page",
        // The transcript's scroll box, and the dock that stays put beneath it.
        "chat-scroll",
        "chat-dock",
        // The entry box, and the row inside it a trait's controls land in
        // (`ComposerControl` in `agentChat.ts`).
        "chat-composer",
        "chat-composer-controls",
    ] {
        assert!(
            screen.contains(&format!("\"{class}")),
            "AgentChat.tsx should render `.{class}`"
        );
        assert!(
            css.contains(&format!(".{class}")),
            "admin.css has no rules for `.{class}`, which AgentChat.tsx renders"
        );
    }

    // The wrapper is a flex column only for this screen, selected by what it
    // contains — the alternative was a prop threaded through the router for one
    // route. Without it the scroll box has no height to be `flex: 1` of.
    assert!(
        css.contains(".page-wrapper:has(> .chat-page)"),
        "admin.css should give the page wrapper a fixed height when it holds a chat"
    );
}

/// A chat can be popped out of its page into a window in the bottom-right
/// corner, which the rest of the admin is then navigated underneath. That is
/// three files agreeing — the store (`chatWindows.ts`), the windows
/// (`PoppedChats.tsx`) and the chat itself, which renders both the page and the
/// inside of a window — and two of the agreements fail silently.
#[test]
fn a_popped_out_chat_has_its_window_and_its_rules() {
    let css = read("src/admin.css");
    let windows = read("src/PoppedChats.tsx");
    let screen = read("src/screens/AgentChat.tsx");

    // The corner is furniture of the whole shell, not of a route: rendered by
    // `App.tsx` outside the routed screen, which is the only reason a window
    // survives navigating away from the chat it was popped out of.
    let app = read("src/App.tsx");
    assert!(
        app.contains("<PoppedChats />"),
        "App.tsx should render the popped-out chats outside the routed screen"
    );

    for (file, source, class) in [
        // The row along the bottom, and one window in it.
        ("PoppedChats.tsx", &windows, "chat-window-row"),
        ("PoppedChats.tsx", &windows, "chat-window"),
        // The dimmed page behind a full-screen chat.
        ("PoppedChats.tsx", &windows, "chat-window-backdrop"),
        // The title bar, which is what a minimized chat is reduced to.
        ("AgentChat.tsx", &screen, "chat-window-head"),
        ("AgentChat.tsx", &screen, "chat-window-title"),
        // The transcript and its furniture, shared by the page and the window.
        ("AgentChat.tsx", &screen, "chat-surface"),
    ] {
        assert!(
            source.contains(&format!("\"{class}")) || source.contains(&format!("`{class}")),
            "{file} should render `.{class}`"
        );
        assert!(
            css.contains(&format!(".{class}")),
            "admin.css has no rules for `.{class}`, which {file} renders"
        );
    }

    // A window's mode is a class on a window that never moves in the DOM — each
    // one holds a live socket and an unsaved transcript, so a mode that changed
    // where the window is rendered would drop the conversation to get there.
    assert!(
        windows.contains("chat-window-${chat.mode}"),
        "PoppedChats.tsx should express a window's mode as a class, not as a different tree"
    );
    for mode in ["minimized", "full"] {
        assert!(
            css.contains(&format!(".chat-window-{mode}")),
            "admin.css has no rules for a `{mode}` chat window"
        );
    }
    // Minimizing hides the body; it must not unmount it, and the rule is the
    // half of that promise the markup cannot state.
    assert!(
        css.contains(".chat-window-minimized .chat-surface"),
        "a minimized window should hide its transcript in CSS, keeping the socket alive"
    );

    // `.page:has(.chat-page)` puts the whole admin into the full-height chat
    // layout. A window is inside `.page` on every route, so it takes the shared
    // `.chat-surface` and leaves `.chat-page` to the screen that really is one.
    // Whitespace-insensitive: the choice is a ternary that rustfmt's cousin,
    // prettier, is free to wrap.
    let flat = screen.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains(r#"const surfaceClass = frame ? "chat-surface" :"#),
        "a popped-out chat must not carry `.chat-page` — it would claim the viewport \
         on whatever screen it is floating over"
    );
}

#[test]
fn the_logo_is_vendored_and_not_auto_darkened() {
    let logo = read("src/vendor/saltcorn-logo.svg");
    assert!(
        logo.starts_with("<svg") && logo.contains("viewBox"),
        "the vendored logo should be an SVG with a viewBox (so it scales)"
    );

    let icons = read("src/icons.tsx");
    assert!(
        icons.contains("./vendor/saltcorn-logo.svg"),
        "icons.tsx should import the vendored logo rather than redrawing a mark"
    );

    // Tabler's `.navbar-brand-autodark` applies `brightness(0) invert(1)` to the
    // brand image, which turns this three-colour logo into a white silhouette.
    // The class is on Tabler's own templates, so it is exactly the thing a
    // copy-paste from them would reintroduce.
    let app = read("src/App.tsx");
    for (file, source) in [
        ("App.tsx", &app),
        ("screens/Login.tsx", &read("src/screens/Login.tsx")),
        ("screens/FirstUser.tsx", &read("src/screens/FirstUser.tsx")),
    ] {
        assert!(
            source.contains("SaltcornLogo"),
            "{file} should show the logo via <SaltcornLogo>"
        );
        // Only in a `className`: the class is named in prose in these files
        // (explaining why it is absent), and that mention is not a use of it.
        let applied = source
            .lines()
            .any(|line| line.contains("className") && line.contains("navbar-brand-autodark"));
        assert!(
            !applied,
            "{file} must not apply `navbar-brand-autodark` — it would flatten the logo to white"
        );
    }
}

#[test]
fn the_spa_loads_the_theme() {
    let main = read("src/main.tsx");
    assert!(
        main.contains("./vendor/tabler/tabler.min.css"),
        "main.tsx should import the vendored Tabler stylesheet"
    );
    // Tabler's CSS *is* Bootstrap's plus the theme, so loading Bootstrap's own
    // stylesheet as well would be dead weight that also overrides parts of it
    // depending on import order.
    assert!(
        !main.contains("bootstrap/dist/css"),
        "main.tsx should not also import Bootstrap's stylesheet — Tabler supersedes it"
    );

    // The layout the admin shell is built on: Tabler's vertical layout is a
    // `.navbar-vertical` sidebar and a `.page-wrapper` beside it, and the two
    // are load-bearing (the sidebar's width is what offsets the wrapper).
    let app = read("src/App.tsx");
    for class in ["navbar-vertical", "page-wrapper"] {
        assert!(
            app.contains(class),
            "App.tsx should render Tabler's vertical layout (missing `{class}`)"
        );
    }

    // Every screen states its title through the shared page header rather than
    // an ad-hoc heading, so the header furniture stays in one place.
    for screen in [
        "Tables",
        "Applications",
        "Triggers",
        "FileStores",
        "FileManager",
        "Users",
        "Roles",
        "TableDetail",
        "ApplicationForm",
        "FileStoreForm",
        "TriggerForm",
    ] {
        let source = read(&format!("src/screens/{screen}.tsx"));
        assert!(
            source.contains("<PageHeader"),
            "{screen}.tsx should render its title with <PageHeader>"
        );
    }
}
