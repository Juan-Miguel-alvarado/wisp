//! Rendering. Each function mirrors one of yazi's UI components (`Header`,
//! `Tabs`, `Entity`, `Linemode`, `Rail`, `Marker`, `Status`, `Progress`, and
//! the modal layers) so the screen looks the same and themes apply the same
//! way. Everything draws straight into the frame buffer; only the rows that
//! are visible are ever touched, so huge directories cost nothing extra.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Widget};
use ratatui::Frame;
use ratatui_image::Image;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Level, Origin};
use crate::folder::{Folder, Stage};
use crate::fs::File;
use crate::input::Mode as InputMode;
use crate::preview::PreviewContent;
use crate::tab::Tab;
use crate::theme::flags;
use crate::util::{
    linemode_time, perm_string, printable, readable_path, readable_size, truncate_left, width,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Parent,
    Current,
    Preview,
}

/// Write `s` at (x, y), clipped to `max` cells. Returns the cells used.
fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, max: usize, style: Style) -> usize {
    if max == 0 || s.is_empty() {
        return 0;
    }
    let (nx, _) = buf.set_stringn(x, y, s, max, style);
    (nx - x) as usize
}

/// Base style used by yazi for status/tabs: fg/bg reset, patched by theme.
fn reset_patch(s: Style) -> Style {
    Style::default().fg(Color::Reset).bg(Color::Reset).patch(s)
}

fn bg_of(s: Style) -> Color {
    s.bg.unwrap_or(Color::Reset)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if app.lua_ui_active() {
        let snap = app.plugin_snapshot_light();
        if let Some(rt) = app.lua.as_ref() {
            rt.refresh_cx(&snap);
        }
    }
    {
        let buf = f.buffer_mut();
        buf.set_style(area, app.th.s("app.overall"));
        header(buf, app);
        tabs(buf, app);
        body(buf, app);
        lua_borders(buf, app);
        if !app.status_hidden {
            status(buf, app);
        }
    }

    // Image previews render through their protocol widget.
    if let Some(c) = app.preview_content.clone() {
        if let PreviewContent::Image(proto) = &*c {
            let hovered_ok = app
                .hovered()
                .is_some_and(|h| app.preview_key.as_ref().is_some_and(|k| k.url == h.url));
            if hovered_ok && app.areas.preview.width > 0 {
                let a = app.areas.preview;
                f.render_widget(Image::new(proto), a);
            }
        }
    }

    let buf = f.buffer_mut();
    if app.help.is_some() {
        help(buf, app);
    }
    if app.tasks_view.is_some() {
        tasks(buf, app);
    }
    if app.spot.is_some() {
        spot(buf, app);
    }
    if app.pick.is_some() {
        pick(buf, app);
    }
    if app.confirm.is_some() {
        confirm(buf, app);
    }
    if let Some(rt) = app.lua.as_ref().filter(|rt| rt.modal_active()) {
        // Plugin UIs registered as Modal children (e.g. `mount`).
        let draws = rt.modal_draws(app.size.0, app.size.1);
        render_draws(buf, &draws);
    }
    if app.which.as_ref().is_some_and(|w| !w.silent) {
        which(buf, app);
    }
    notifications(buf, app);
    if let Some(pos) = input(buf, app) {
        f.set_cursor_position(pos);
    }
}

// --------------------------------------------------------------------------
// Header / tabs / status
// --------------------------------------------------------------------------

type Parts = Vec<(String, Style)>;

/// Draw positioned widgets produced by Lua (plugin modals, list overrides).
fn render_draws(buf: &mut Buffer, draws: &[crate::plugin::Draw]) {
    use crate::plugin::Draw;
    let screen = buf.area;
    for d in draws {
        match d {
            Draw::Clear(r) => Clear.render(r.intersection(screen), buf),
            Draw::Border {
                area,
                kind,
                style,
                title,
            } => {
                let r = area.intersection(screen);
                if r.width < 2 || r.height < 2 {
                    continue;
                }
                let bt = match kind {
                    1 => BorderType::Rounded,
                    2 => BorderType::Double,
                    3 => BorderType::Thick,
                    _ => BorderType::Plain,
                };
                let mut b = Block::default()
                    .borders(Borders::ALL)
                    .border_type(bt)
                    .border_style(*style);
                if let Some((spans, align)) = title {
                    let line = ratatui::text::Line::from(
                        spans
                            .iter()
                            .map(|(t, st)| ratatui::text::Span::styled(t.clone(), *st))
                            .collect::<Vec<_>>(),
                    );
                    b = b.title(match align {
                        1 => line.centered(),
                        2 => line.right_aligned(),
                        _ => line,
                    });
                }
                b.render(r, buf);
            }
            Draw::Bar {
                area,
                symbol,
                style,
            } => {
                let r = area.intersection(screen);
                for y in r.y..r.y + r.height {
                    put(buf, r.x, y, symbol, 1, *style);
                }
            }
            Draw::Lines { area, rows, align } => {
                let r = area.intersection(screen);
                for (i, (spans, fill)) in rows.iter().enumerate() {
                    let y = r.y + i as u16;
                    if y >= r.y + r.height {
                        break;
                    }
                    if *fill != Style::default() {
                        buf.set_style(Rect::new(r.x, y, r.width, 1), *fill);
                    }
                    let w: usize = spans.iter().map(|(t, _)| width(t)).sum();
                    let pad = (r.width as usize).saturating_sub(w);
                    let mut x = r.x
                        + match align {
                            1 => pad / 2,
                            2 => pad,
                            _ => 0,
                        } as u16;
                    for (t, st) in spans {
                        let n = put(
                            buf,
                            x,
                            y,
                            t,
                            (r.x + r.width).saturating_sub(x) as usize,
                            *st,
                        );
                        x += n as u16;
                    }
                }
            }
        }
    }
}

/// yazi's component model: built-in slots `(order, method, parts)`, replaced
/// by a Lua override of the same method and interleaved with Lua children.
fn compose(app: &App, comp: &str, side: i64, area: Rect, slots: Vec<(i64, &str, Parts)>) -> Parts {
    let Some(rt) = app.lua.as_ref().filter(|rt| rt.component_active(comp)) else {
        return slots.into_iter().flat_map(|s| s.2).collect();
    };
    let mut groups: Vec<(i64, Parts)> = slots
        .into_iter()
        .map(|(o, m, parts)| {
            let parts = rt
                .component_override(comp, m, None, Some(area))
                .unwrap_or(parts);
            (o, parts)
        })
        .collect();
    groups.extend(rt.component_children(comp, None, Some(area), Some(side)));
    groups.sort_by_key(|g| g.0);
    groups.into_iter().flat_map(|g| g.1).collect()
}

/// Borders from a Lua Tab layout, with `merge` joining crossings the way
/// yazi does (`┬`, `┴`, `├`, `┤`, `┼`).
fn lua_borders(buf: &mut Buffer, app: &App) {
    if app.lua_borders.is_empty() {
        return;
    }
    let screen = buf.area;
    let mut drawn: Vec<(Rect, bool)> = Vec::new();
    for (r, kind, style, merge) in &app.lua_borders {
        let r = r.intersection(screen);
        if r.width < 2 || r.height < 2 {
            continue;
        }
        let bt = match kind {
            1 => BorderType::Rounded,
            2 => BorderType::Double,
            3 => BorderType::Thick,
            _ => BorderType::Plain,
        };
        Block::default()
            .borders(Borders::ALL)
            .border_type(bt)
            .border_style(*style)
            .render(r, buf);
        drawn.push((r, *merge));
    }
    // Where an inner border's corner sits on an outer border line, draw the
    // junction instead of a corner.
    for (outer, merge) in &drawn {
        if !merge {
            continue;
        }
        for (inner, _) in &drawn {
            if inner == outer {
                continue;
            }
            for x in [inner.x, inner.x + inner.width - 1] {
                if x > outer.x && x < outer.x + outer.width - 1 {
                    if inner.y == outer.y {
                        buf[(x, outer.y)].set_symbol("┬");
                    }
                    if inner.y + inner.height == outer.y + outer.height {
                        buf[(x, outer.y + outer.height - 1)].set_symbol("┴");
                    }
                }
            }
        }
    }
}

fn header(buf: &mut Buffer, app: &App) {
    let a = app.areas.header;
    if a.width == 0 {
        return;
    }
    let tab = app.tab();
    let th = &app.th;

    // Right: selected / yanked count.
    let selected = tab.selected.len();
    let yanked = if selected > 0 {
        0
    } else {
        app.yanked.paths.len()
    };
    let (count, cstyle) = if selected > 0 {
        (format!(" {selected} "), th.s("mgr.count_selected"))
    } else if yanked > 0 {
        let k = if app.yanked.cut {
            "mgr.count_cut"
        } else {
            "mgr.count_copied"
        };
        (format!(" {yanked} "), th.s(k))
    } else {
        (String::new(), Style::default())
    };
    let count_parts: Parts = if count.is_empty() {
        Vec::new()
    } else {
        vec![(count, cstyle), (" ".into(), Style::default())]
    };
    let right = compose(app, "Header", 1, a, vec![(1000, "count", count_parts)]);
    let right_w: usize = right.iter().map(|(s, _)| width(s)).sum();
    if right_w > 0 {
        let mut x = a.x + a.width.saturating_sub(right_w as u16);
        for (s, st) in &right {
            let n = put(
                buf,
                x,
                a.y,
                s,
                (a.x + a.width).saturating_sub(x) as usize,
                *st,
            );
            x += n as u16;
        }
    }

    // Left: cwd plus search/filter/find flags.
    let max = (a.width as usize).saturating_sub(right_w);
    if max == 0 {
        return;
    }
    let mut s = readable_path(&tab.cwd);
    let mut flags = Vec::new();
    if let Some(se) = &tab.search {
        flags.push(format!("search: {}", se.query));
    }
    if let Some(fl) = &tab.current().filter {
        flags.push(format!("filter: {}", fl.query));
    }
    if let Some(fi) = &tab.finder {
        flags.push(format!("find: {}", fi.query));
    }
    if !flags.is_empty() {
        s.push_str(&format!(" ({})", flags.join(", ")));
    }
    let s = truncate_left(&printable(&s), max);
    let left = compose(
        app,
        "Header",
        0,
        a,
        vec![(1000, "cwd", vec![(s, th.s("mgr.cwd"))])],
    );
    let mut x = a.x;
    for (s, st) in &left {
        let n = put(buf, x, a.y, s, max.saturating_sub((x - a.x) as usize), *st);
        x += n as u16;
    }
}

fn tabs(buf: &mut Buffer, app: &App) {
    let Some(a) = app.areas.tabs else { return };
    let th = &app.th;
    let active = reset_patch(th.s("tabs.active"));
    let inactive = reset_patch(th.s("tabs.inactive"));
    let (si_o, si_c) = (
        th.str("tabs.sep_inner.open", ""),
        th.str("tabs.sep_inner.close", ""),
    );
    let (so_o, so_c) = (
        th.str("tabs.sep_outer.open", ""),
        th.str("tabs.sep_outer.close", ""),
    );
    let seps = width(si_o) + width(si_c) + width(so_o) + width(so_c);
    let inner = (a.width as usize).saturating_sub(seps);
    let max = inner / app.tabs.len().max(1);
    let mut x = a.x;
    let end = a.x + a.width;
    let w = |buf: &mut Buffer, s: &str, st: Style, x: &mut u16| {
        let used = put(buf, *x, a.y, s, (end - *x) as usize, st);
        *x += used as u16;
    };
    w(buf, so_o, Style::default().fg(bg_of(inactive)), &mut x);
    for (i, t) in app.tabs.iter().enumerate() {
        let name = format!(" {} {} ", i + 1, t.title());
        let name = crate::util::truncate(&name, max).to_string();
        if i == app.cur {
            w(
                buf,
                si_o,
                Style::default().fg(bg_of(active)).bg(bg_of(inactive)),
                &mut x,
            );
            w(buf, &name, active, &mut x);
            w(
                buf,
                si_c,
                Style::default().fg(bg_of(active)).bg(bg_of(inactive)),
                &mut x,
            );
        } else {
            w(buf, &name, inactive, &mut x);
        }
    }
    w(buf, so_c, Style::default().fg(bg_of(inactive)), &mut x);
}

fn mode_styles(app: &App) -> (Style, Style) {
    let th = &app.th;
    let (m, a) = match app.tab().mode {
        crate::tab::Mode::Select(_) => ("mode.select_main", "mode.select_alt"),
        crate::tab::Mode::Unset(_) => ("mode.unset_main", "mode.unset_alt"),
        crate::tab::Mode::Normal => ("mode.normal_main", "mode.normal_alt"),
    };
    (reset_patch(th.s(m)), reset_patch(th.s(a)))
}

fn status(buf: &mut Buffer, app: &App) {
    let a = app.areas.status;
    if a.width == 0 {
        return;
    }
    let th = &app.th;
    buf.set_style(a, th.s("status.overall"));
    let (main, alt) = mode_styles(app);
    let (sl_o, sl_c) = (
        th.str("status.sep_left.open", ""),
        th.str("status.sep_left.close", ""),
    );
    let (sr_o, sr_c) = (
        th.str("status.sep_right.open", ""),
        th.str("status.sep_right.close", ""),
    );
    let folder = app.tab().current();
    let h = folder.hovered();

    // Right side first, to know how much room the left has.
    let mut perm: Parts = Vec::new();
    if let Some(h) = h {
        for c in perm_string(h.cha.mode).chars() {
            let k = match c {
                '-' | '?' => "status.perm_sep",
                'r' => "status.perm_read",
                'w' => "status.perm_write",
                'x' | 's' | 'S' | 't' | 'T' => "status.perm_exec",
                _ => "status.perm_type",
            };
            perm.push((c.to_string(), th.s(k)));
        }
    }
    let len = folder.len();
    let cursor = folder.cursor;
    let percent = if cursor == 0 || len == 0 {
        0
    } else {
        (cursor + 1) * 100 / len
    };
    let pct = match percent {
        0 => " Top ".to_string(),
        100 => " Bot ".to_string(),
        p => format!(" {p:2}% "),
    };
    let percent_parts: Parts = vec![
        (format!(" {sr_o}"), Style::default().fg(bg_of(alt))),
        (pct, alt),
    ];
    let position: Parts = vec![
        (
            sr_o.to_string(),
            Style::default().fg(bg_of(main)).bg(bg_of(alt)),
        ),
        (format!(" {:2}/{:<2} ", (cursor + 1).min(len), len), main),
        (
            sr_c.to_string(),
            Style::default().fg(bg_of(main)).bg(Color::Reset),
        ),
    ];
    let right = compose(
        app,
        "Status",
        1,
        a,
        vec![
            (1000, "perm", perm),
            (2000, "percent", percent_parts),
            (3000, "position", position),
        ],
    );
    let right_w: usize = right.iter().map(|(s, _)| width(s)).sum();
    let mut x = a.x + a.width.saturating_sub(right_w as u16);
    for (s, st) in &right {
        let used = put(
            buf,
            x,
            a.y,
            s,
            (a.x + a.width).saturating_sub(x) as usize,
            *st,
        );
        x += used as u16;
    }

    // Left: mode, size, name.
    let mode = app.tab().mode.label();
    let mode_parts: Parts = vec![
        (
            sl_o.to_string(),
            Style::default().fg(bg_of(main)).bg(Color::Reset),
        ),
        (format!(" {mode} "), main),
        (
            sl_c.to_string(),
            Style::default().fg(bg_of(main)).bg(bg_of(alt)),
        ),
    ];
    let size = h
        .map(|f| {
            if f.is_dir() {
                app.dir_sizes.get(&f.url).map(|s| s.0).unwrap_or(f.cha.len)
            } else {
                f.cha.len
            }
        })
        .unwrap_or(0);
    let length: Parts = vec![
        (format!(" {} ", readable_size(size)), alt),
        (sl_c.to_string(), Style::default().fg(bg_of(alt))),
    ];
    let name: Parts = h
        .map(|h| vec![(format!(" {}", printable(&h.name)), Style::default())])
        .unwrap_or_default();
    let left = compose(
        app,
        "Status",
        0,
        a,
        vec![
            (1000, "mode", mode_parts),
            (2000, "length", length),
            (3000, "name", name),
        ],
    );
    let maxl = (a.width as usize).saturating_sub(right_w + 1);
    let mut x = a.x;
    for (s, st) in &left {
        let rem = maxl.saturating_sub((x - a.x) as usize);
        let used = put(buf, x, a.y, s, rem, *st);
        x += used as u16;
    }

    progress(buf, app, a, right_w);
}

fn progress(buf: &mut Buffer, app: &App, a: Rect, right_w: usize) {
    let running: Vec<_> = app
        .tasks
        .iter()
        .filter(|t| t.state == crate::tasks::State::Running)
        .collect();
    if running.is_empty() {
        return;
    }
    let (mut done, mut total) = (0u64, 0u64);
    for t in &running {
        if t.bytes_total > 0 {
            done += t.bytes_done;
            total += t.bytes_total;
        } else {
            done += t.items_done;
            total += t.items_total.max(1);
        }
    }
    let pct = (done * 100).checked_div(total).unwrap_or(0).min(100);
    let w = ((a.width as usize).saturating_sub(right_w + 1)).min(20) as u16;
    if w < 8 {
        return;
    }
    let x = a.x + a.width.saturating_sub(right_w as u16 + 21);
    let failed = app
        .tasks
        .iter()
        .any(|t| matches!(t.state, crate::tasks::State::Failed(_)));
    let th = &app.th;
    let style = if failed {
        th.s("status.progress_error")
    } else {
        th.s("status.progress_normal")
    };
    // Like yazi: a percentage only when some task has measurable progress.
    let measurable = running
        .iter()
        .any(|t| t.bytes_total > 0 || t.items_total > 0);
    let label = if measurable {
        format!("{pct:3}%, {} left", running.len())
    } else {
        format!("{} left", running.len())
    };
    let filled = (w as u64 * pct / 100) as u16;
    // Gauge: filled cells take the fg colour as background.
    let fill = Style::default()
        .bg(style.fg.unwrap_or(Color::Green))
        .fg(style.bg.unwrap_or(Color::Black));
    let empty = Style::default()
        .bg(style.bg.unwrap_or(Color::Gray))
        .fg(style.fg.unwrap_or(Color::Green));
    let label_w = width(&label) as u16;
    let lx = x + w.saturating_sub(label_w) / 2;
    for i in 0..w {
        let cell = &mut buf[(x + i, a.y)];
        cell.set_symbol(" ");
        cell.set_style(if i < filled { fill } else { empty });
    }
    for (i, ch) in label.chars().enumerate() {
        let cx = lx + i as u16;
        if cx >= x + w {
            break;
        }
        let cell = &mut buf[(cx, a.y)];
        cell.set_char(ch);
        cell.set_style(th.s("status.progress_label"));
    }
}

// --------------------------------------------------------------------------
// Body: parent | current | preview, rails and markers
// --------------------------------------------------------------------------

fn body(buf: &mut Buffer, app: &App) {
    let tab = app.tab();
    let a = app.areas;
    let th = &app.th;

    if let Some(p) = tab.parent() {
        if a.parent.width > 0 {
            match app
                .lua
                .as_ref()
                .filter(|rt| rt.component_active("Parent") && rt.list_custom("Parent"))
            {
                Some(rt) => render_draws(buf, &rt.list_redraw("Parent", a.parent)),
                None => folder_list(buf, app, tab, p, a.parent, Pane::Parent),
            }
        }
    }
    folder_current(buf, app, tab, a.current);
    preview(buf, app, tab, a.preview);

    // Rails.
    let sym = th.str("mgr.border_symbol", "│");
    let bst = th.s("mgr.border_style");
    let c = a.chunks;
    if c[0].width > 0 && c[1].width > 0 {
        for y in c[1].y..c[1].y + c[1].height {
            put(buf, c[1].x, y, sym, 1, bst);
        }
    }
    if c[2].width > 0 && c[1].width > 0 {
        let x = (c[1].x + c[1].width).saturating_sub(1);
        for y in c[1].y..c[1].y + c[1].height {
            put(buf, x, y, sym, 1, bst);
        }
    }

    // Markers.
    if let Some(p) = tab.parent() {
        markers(buf, app, tab, p, c[0], a.parent.height as usize, false);
    }
    markers(
        buf,
        app,
        tab,
        tab.current(),
        c[1],
        a.current.height as usize,
        true,
    );
}

fn marker_style(app: &App, tab: &Tab, f: &File, idx: usize, in_current: bool) -> Option<Style> {
    let th = &app.th;
    if in_current && tab.is_marked(idx) {
        return match tab.mode {
            crate::tab::Mode::Select(_) => Some(th.s("mgr.marker_marked")),
            _ => None,
        };
    }
    if tab.selected.contains(&f.url) {
        return Some(th.s("mgr.marker_selected"));
    }
    if app.yanked.paths.contains(&f.url) {
        return Some(if app.yanked.cut {
            th.s("mgr.marker_cut")
        } else {
            th.s("mgr.marker_copied")
        });
    }
    None
}

fn markers(
    buf: &mut Buffer,
    app: &App,
    tab: &Tab,
    folder: &Folder,
    chunk: Rect,
    h: usize,
    cur: bool,
) {
    if chunk.width == 0 || chunk.height == 0 {
        return;
    }
    let sym = app.th.str("mgr.marker_symbol", "│");
    for (row, i) in folder.window(h).enumerate() {
        let Some(f) = folder.get(i) else { break };
        if let Some(st) = marker_style(app, tab, f, i, cur) {
            let y = chunk.y + row as u16;
            if y < chunk.y + chunk.height {
                put(buf, chunk.x, y, sym, 1, st);
            }
        }
    }
}

fn centered_text(buf: &mut Buffer, a: Rect, s: &str, style: Style) {
    if a.width == 0 || a.height == 0 {
        return;
    }
    let w = width(s).min(a.width as usize) as u16;
    let x = a.x + (a.width - w) / 2;
    put(buf, x, a.y, s, a.width as usize, style);
}

fn folder_current(buf: &mut Buffer, app: &App, tab: &Tab, a: Rect) {
    if let Some(rt) = app
        .lua
        .as_ref()
        .filter(|rt| rt.component_active("Current") && rt.list_custom("Current"))
    {
        return render_draws(buf, &rt.list_redraw("Current", a));
    }
    let folder = tab.current();
    if folder.is_empty() {
        let s = if folder.filter.is_some() {
            "No filter results".to_string()
        } else {
            match (&folder.stage, &tab.search) {
                (_, Some(s)) if !s.done => "Searching...".into(),
                (Stage::Loading, _) => "Loading...".into(),
                (Stage::Loaded, _) => "No items".into(),
                (Stage::Failed(e), _) => format!("Error: {e}"),
            }
        };
        centered_text(buf, a, &s, Style::default());
        return;
    }
    folder_list(buf, app, tab, folder, a, Pane::Current);
}

fn linemode(app: &App, tab: &Tab, f: &File) -> String {
    match tab.pref.linemode.as_str() {
        "size" => {
            if f.is_dir() {
                if let Some((s, _)) = app.dir_sizes.get(&f.url) {
                    return readable_size(*s);
                }
                match tab.folders.get(&f.url) {
                    Some(d) if d.stage == Stage::Loaded => d.len().to_string(),
                    _ => String::new(),
                }
            } else {
                readable_size(f.cha.len)
            }
        }
        "mtime" => linemode_time(f.cha.mtime),
        "btime" => linemode_time(f.cha.btime),
        "permissions" => perm_string(f.cha.mode),
        "owner" => format!(
            "{}:{}",
            crate::util::user_name(f.cha.uid),
            crate::util::group_name(f.cha.gid)
        ),
        "none" | "solo" | "" => String::new(),
        other => other.to_string(),
    }
}

fn folder_list(buf: &mut Buffer, app: &App, tab: &Tab, folder: &Folder, a: Rect, pane: Pane) {
    if a.width == 0 || a.height == 0 {
        return;
    }
    let th = &app.th;
    let search_root = if pane == Pane::Current && tab.search.is_some() {
        Some(tab.cwd.as_path())
    } else {
        None
    };
    // Finder matches, for the `[i/n]` indicator.
    let (found_idx, found_n) = match (&tab.finder, pane) {
        (Some(p), Pane::Current) => {
            let mut n = 0usize;
            let mut idx = None;
            for (i, f) in folder.files().enumerate() {
                if p.is_match(&f.name) {
                    if i == folder.cursor {
                        idx = Some(n);
                    }
                    n += 1;
                    if n > 100 && idx.is_some() {
                        break;
                    }
                }
            }
            (idx, n)
        }
        _ => (None, 0),
    };
    let indicator = match pane {
        Pane::Current => th.s("indicator.current"),
        Pane::Preview => th.s("indicator.preview"),
        Pane::Parent => th.s("indicator.parent"),
    };
    let pad_open = th.str("indicator.padding.open", "");
    let pad_close = th.str("indicator.padding.close", "");

    for (row, i) in folder.window(a.height as usize).enumerate() {
        let Some(f) = folder.get(i) else { break };
        let y = a.y + row as u16;
        let hovered = i == folder.cursor;
        let mut fl = f.cha.flags;
        if hovered {
            fl |= flags::HOVERED;
        }
        let mime = app.mime(f);
        let base = th.file_style(&f.url, &mime, fl);
        let style = if hovered { base.patch(indicator) } else { base };
        let row_rect = Rect::new(a.x, y, a.width, 1);
        buf.set_style(row_rect, style);

        // Lua components see the file as yazi's `self._file`.
        let lua = app.lua.as_ref();
        let ctx = crate::plugin::FileCtx {
            file: f,
            hovered,
            in_current: pane == Pane::Current,
            in_preview: pane == Pane::Preview,
            in_parent: pane == Pane::Parent,
        };
        let lm_lua = lua.filter(|rt| rt.component_active("Linemode"));
        let en_lua = lua.filter(|rt| rt.component_active("Entity"));

        // Right side: linemode (current pane), Lua linemode children, padding.
        let mut right: Vec<(i64, Parts)> = Vec::new();
        if pane == Pane::Current {
            let mode = tab.pref.linemode.as_str();
            let custom =
                lm_lua.and_then(|rt| rt.component_override("Linemode", mode, Some(&ctx), None));
            let solo: Parts = match custom {
                Some(sp) if !sp.is_empty() && sp.iter().any(|x| !x.0.is_empty()) => {
                    let mut v = vec![(" ".to_string(), Style::default())];
                    v.extend(sp);
                    v
                }
                Some(_) => Vec::new(),
                None => {
                    let lm = linemode(app, tab, f);
                    if lm.is_empty() {
                        Vec::new()
                    } else {
                        vec![(format!(" {lm}"), Style::default())]
                    }
                }
            };
            right.push((1000, solo));
        }
        if let Some(rt) = lm_lua {
            right.extend(rt.component_children("Linemode", Some(&ctx), None, None));
        }
        let rev = if hovered { style_rev(style) } else { None };
        match rev {
            Some(r) => right.push((2000, vec![(pad_close.to_string(), r)])),
            None => right.push((2000, vec![(" ".into(), Style::default())])),
        }
        right.sort_by_key(|g| g.0);
        let right: Parts = right.into_iter().flat_map(|g| g.1).collect();
        let right_w: usize = right.iter().map(|(s, _)| width(s)).sum();

        // Left side: padding, icon, prefix, name, found, symlink.
        let mut left: Vec<(String, Style)> = Vec::new();
        match rev {
            Some(r) => left.push((pad_open.to_string(), r)),
            None => left.push((" ".into(), Style::default())),
        }
        if let Some(icon) = th.icons.get(&f.url, &f.name, fl) {
            let st = if hovered {
                Style::default()
            } else {
                icon.style
            };
            left.push((format!("{} ", icon.text), st));
        }
        if let Some(root) = search_root {
            if let Some(p) = crate::search::prefix_of(root, &f.url) {
                left.push((format!("{}/", printable(&p)), Style::default()));
            }
        }
        let name = printable(&f.name);
        let hl = match (&tab.finder, pane) {
            (Some(p), Pane::Current) => p.highlights(&name),
            _ => Vec::new(),
        };
        let name_idx = left.len();
        if hl.is_empty() {
            left.push((name.to_string(), Style::default()));
        } else {
            let kw = th.s("mgr.find_keyword");
            let mut last = 0;
            for (s, e) in hl {
                if s > last {
                    left.push((name[last..s].to_string(), Style::default()));
                }
                left.push((name[s..e].to_string(), kw));
                last = e;
            }
            if last < name.len() {
                left.push((name[last..].to_string(), Style::default()));
            }
        }
        if hovered {
            if let Some(k) = found_idx {
                if k < 99 {
                    let n = if found_n >= 100 {
                        "99+".to_string()
                    } else {
                        found_n.to_string()
                    };
                    left.push(("  ".into(), Style::default()));
                    left.push((format!("[{}/{}]", k + 1, n), th.s("mgr.find_position")));
                }
            }
        }
        if app.cfg.mgr.show_symlink {
            if let Some(t) = &f.link_to {
                left.push((
                    format!(" -> {}", printable(&t.to_string_lossy())),
                    th.s("mgr.symlink_target"),
                ));
            }
        }

        // Lua `Entity` children go between the built-in slots by order
        // (padding 1000, icon 2000, prefix 3000, name 4000, found 5000,
        // symlink 6000).
        let mut name_idx = name_idx;
        if let Some(rt) = en_lua {
            let kids = rt.component_children("Entity", Some(&ctx), None, None);
            for (order, spans) in kids.into_iter().rev() {
                let at = match order {
                    o if o < 1000 => 0,
                    o if o < 2000 => 1.min(left.len()),
                    o if o < 4000 => name_idx,
                    _ => left.len(),
                };
                if at <= name_idx && order < 4000 {
                    name_idx += spans.len();
                }
                for (k, sp) in spans.into_iter().enumerate() {
                    left.insert(at + k, sp);
                }
            }
        }

        let max = (a.width as usize).saturating_sub(right_w);
        let left_w: usize = left.iter().map(|(s, _)| width(s)).sum();
        if left_w > max {
            // Truncate, keeping the extension visible: `long_na….rs`.
            let ell = match f.ext() {
                Some(e) if !f.is_dir() && width(e) + 3 < max => format!("….{e}"),
                _ => "…".to_string(),
            };
            let budget = max.saturating_sub(width(&ell));
            let mut used = 0;
            let mut x = a.x;
            for (k, (s, st)) in left.iter().enumerate() {
                let w = width(s);
                if used + w > budget {
                    let part = crate::util::truncate(s, budget - used);
                    let n = put(buf, x, y, part, budget - used, *st);
                    x += n as u16;
                    let est = if k >= name_idx { *st } else { Style::default() };
                    put(buf, x, y, &ell, width(&ell), est);
                    break;
                }
                let n = put(buf, x, y, s, w, *st);
                x += n as u16;
                used += n;
            }
        } else {
            let mut x = a.x;
            for (s, st) in &left {
                let n = put(buf, x, y, s, max.saturating_sub((x - a.x) as usize), *st);
                x += n as u16;
            }
        }
        let mut x = a.x + a.width - right_w.min(a.width as usize) as u16;
        for (s, st) in &right {
            let n = put(buf, x, y, s, (a.x + a.width - x) as usize, *st);
            x += n as u16;
        }
    }
}

/// yazi's `Entity:style_rev`: the colour of the highlight bar, reversed, for
/// the rounded caps around the hovered row.
fn style_rev(s: Style) -> Option<Style> {
    if let Some(bg) = s.bg {
        if bg != Color::Reset {
            return Some(
                Style::default()
                    .fg(bg)
                    .bg(Color::Reset)
                    .add_modifier(Modifier::REVERSED),
            );
        }
    }
    if s.add_modifier.contains(Modifier::REVERSED) {
        return Some(
            Style::default()
                .bg(Color::Reset)
                .add_modifier(Modifier::REVERSED),
        );
    }
    None
}

fn preview(buf: &mut Buffer, app: &App, tab: &Tab, a: Rect) {
    if a.width == 0 || a.height == 0 {
        return;
    }
    let Some(h) = tab.hovered() else { return };
    if h.is_dir() {
        match tab.folders.get(&h.url) {
            Some(f) if !f.is_empty() => folder_list(buf, app, tab, f, a, Pane::Preview),
            Some(f) => {
                let s = match &f.stage {
                    Stage::Loading => "Loading...".to_string(),
                    Stage::Loaded => "No items".to_string(),
                    Stage::Failed(e) => format!("Error: {e}"),
                };
                centered_text(buf, a, &s, Style::default());
            }
            None => {}
        }
        return;
    }
    let Some(c) = &app.preview_content else {
        return;
    };
    if app.preview_key.as_ref().map(|k| &k.url) != Some(&h.url) {
        return;
    }
    match &**c {
        PreviewContent::Text { lines, .. } => {
            for (row, line) in lines.iter().take(a.height as usize).enumerate() {
                let y = a.y + row as u16;
                let mut x = a.x;
                for span in &line.spans {
                    let rem = (a.x + a.width).saturating_sub(x) as usize;
                    if rem == 0 {
                        break;
                    }
                    let n = put(buf, x, y, &span.content, rem, span.style);
                    x += n as u16;
                }
            }
        }
        PreviewContent::Error(e) => {
            put(
                buf,
                a.x,
                a.y,
                e,
                a.width as usize,
                Style::default().fg(Color::Red),
            );
        }
        PreviewContent::Image(_) | PreviewContent::Empty => {}
    }
}

// --------------------------------------------------------------------------
// Popups
// --------------------------------------------------------------------------

fn popup_block<'a>(title: &'a str, border: Style, title_style: Style) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(ratatui::text::Span::styled(title, title_style))
}

fn clamp_rect(r: Rect, screen: Rect) -> Rect {
    let w = r.width.min(screen.width);
    let h = r.height.min(screen.height);
    let x = r.x.min(screen.width.saturating_sub(w));
    let y = r.y.min(screen.height.saturating_sub(h));
    Rect::new(x, y, w, h)
}

fn screen(app: &App) -> Rect {
    Rect::new(0, 0, app.size.0, app.size.1)
}

fn center(app: &App, w: u16, h: u16) -> Rect {
    let s = screen(app);
    let w = w.min(s.width);
    let h = h.min(s.height);
    Rect::new((s.width - w) / 2, (s.height - h) / 2, w, h)
}

/// Row of the hovered file in the current pane.
fn hovered_rect(app: &App) -> Rect {
    let a = app.areas.current;
    let f = app.tab().current();
    let row = f.cursor.saturating_sub(f.offset) as u16;
    Rect::new(a.x, a.y + row.min(a.height.saturating_sub(1)), a.width, 1)
}

fn input(buf: &mut Buffer, app: &mut App) -> Option<ratatui::layout::Position> {
    let scr = screen(app);
    let hov = hovered_rect(app);
    let App { th, input, .. } = app;
    let th = &*th;
    let st = input.as_mut()?;
    let w = 50u16.min(scr.width);
    let rect = match st.origin {
        Origin::TopCenter => Rect::new((scr.width.saturating_sub(w)) / 2, 2, w, 3),
        Origin::Hovered => {
            let y = if hov.y + 4 <= scr.height {
                hov.y + 1
            } else {
                hov.y.saturating_sub(3)
            };
            Rect::new(hov.x, y, w, 3)
        }
    };
    let rect = clamp_rect(rect, scr);
    Clear.render(rect, buf);
    let block = popup_block(&st.title, th.s("input.border"), th.s("input.title"));
    let inner = block.inner(rect);
    block.render(rect, buf);
    let view = st.input.view(inner.width.saturating_sub(1) as usize);
    put(
        buf,
        inner.x,
        inner.y,
        &view.text,
        inner.width as usize,
        th.s("input.value"),
    );
    if let Some((s, e)) = view.selection {
        let sel = th.s("input.selected");
        for x in s..e {
            let cx = inner.x + x as u16;
            if cx < inner.x + inner.width {
                let c = &mut buf[(cx, inner.y)];
                c.set_style(sel);
            }
        }
    }
    let pos = ratatui::layout::Position::new(inner.x + view.cursor_col as u16, inner.y);
    let shape = match st.input.mode {
        InputMode::Insert => crossterm::cursor::SetCursorStyle::SteadyBar,
        InputMode::Replace => crossterm::cursor::SetCursorStyle::SteadyUnderScore,
        _ => crossterm::cursor::SetCursorStyle::SteadyBlock,
    };
    let _ = crossterm::queue!(std::io::stdout(), shape);

    // Completion menu below the input.
    if st.cmp_shown && !st.cmp.is_empty() {
        let n = st.cmp.len().min(10) as u16;
        let r = clamp_rect(
            Rect::new(rect.x, rect.y + rect.height, rect.width, n + 2),
            scr,
        );
        Clear.render(r, buf);
        let b = popup_block("", th.s("cmp.border"), Style::default());
        let inner = b.inner(r);
        b.render(r, buf);
        let off = st.cmp_cursor.saturating_sub(n as usize - 1);
        let icon = th.str("cmp.icon_folder", "");
        for (row, (i, c)) in st
            .cmp
            .iter()
            .enumerate()
            .skip(off)
            .take(n as usize)
            .enumerate()
        {
            let style = if i == st.cmp_cursor {
                th.s("cmp.active")
            } else {
                th.s("cmp.inactive")
            };
            let y = inner.y + row as u16;
            buf.set_style(Rect::new(inner.x, y, inner.width, 1), style);
            put(
                buf,
                inner.x,
                y,
                &format!(" {icon} {c}"),
                inner.width as usize,
                style,
            );
        }
    }
    Some(pos)
}

fn confirm(buf: &mut Buffer, app: &App) {
    let Some(c) = &app.confirm else { return };
    let th = &app.th;
    // yazi's fixed boxes: trash/delete 70x20, quit/overwrite 50x15.
    let (w, h) = match c.kind {
        crate::app::ConfirmKind::Trash(_) | crate::app::ConfirmKind::Delete(_) => (70, 20),
        _ => (50, 15),
    };
    let rect = center(app, w, h);
    Clear.render(rect, buf);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(th.s("confirm.border"))
        .title(ratatui::text::Line::styled(c.title.clone(), th.s("confirm.title")).centered());
    let inner = block.inner(rect);
    block.render(rect, buf);
    if inner.height < 3 {
        return;
    }
    let sep_y = inner.y + inner.height - 2;
    let mut y = inner.y;
    for l in c.body.lines() {
        if y >= sep_y {
            break;
        }
        put(
            buf,
            inner.x + 1,
            y,
            l,
            (inner.width as usize).saturating_sub(1),
            th.s("confirm.body"),
        );
        y += 1;
    }
    if !c.body.is_empty() && !c.list.is_empty() {
        y += 1;
    }
    let room = sep_y.saturating_sub(y) as usize;
    for item in c.list.iter().skip(c.offset).take(room) {
        put(
            buf,
            inner.x + 1,
            y,
            &printable(item),
            (inner.width as usize).saturating_sub(1),
            th.s("confirm.list"),
        );
        y += 1;
    }
    let border = th.s("confirm.border");
    for x in inner.x..inner.x + inner.width {
        put(buf, x, sep_y, "─", 1, border);
    }
    // Buttons.
    let yes = th.str("confirm.btn_labels.0", "  [Y]es  ");
    let no = th.str("confirm.btn_labels.1", "  (N)o  ");
    let by = inner.y + inner.height - 1;
    let half = inner.width / 2;
    let yw = width(yes) as u16;
    let nw = width(no) as u16;
    put(
        buf,
        inner.x + half.saturating_sub(yw) / 2,
        by,
        yes,
        yw as usize,
        th.s("confirm.btn_yes"),
    );
    put(
        buf,
        inner.x + half + half.saturating_sub(nw) / 2,
        by,
        no,
        nw as usize,
        th.s("confirm.btn_no"),
    );
}

fn pick(buf: &mut Buffer, app: &App) {
    let Some(p) = &app.pick else { return };
    let th = &app.th;
    let scr = screen(app);
    let hov = hovered_rect(app);
    let h = (p.items.len() as u16 + 2).clamp(3, 7);
    let y = if hov.y + 1 + h <= scr.height {
        hov.y + 1
    } else {
        hov.y.saturating_sub(h)
    };
    let rect = clamp_rect(Rect::new(hov.x, y, 50, h), scr);
    Clear.render(rect, buf);
    let block = popup_block(&p.title, th.s("pick.border"), Style::default());
    let inner = block.inner(rect);
    block.render(rect, buf);
    let n = inner.height as usize;
    let off = p.cursor.saturating_sub(n.saturating_sub(1));
    for (row, (i, o)) in p.items.iter().enumerate().skip(off).take(n).enumerate() {
        let st = if i == p.cursor {
            th.s("pick.active")
        } else {
            th.s("pick.inactive")
        };
        let label = if i == p.cursor {
            format!("\u{eb70} {}", o.label())
        } else {
            format!("  {}", o.label())
        };
        put(
            buf,
            inner.x,
            inner.y + row as u16,
            &label,
            inner.width as usize,
            st,
        );
    }
}

fn which(buf: &mut Buffer, app: &App) {
    let Some(w) = &app.which else { return };
    let th = &app.th;
    let cols: usize = th.str("which.cols", "3").parse().unwrap_or(3).clamp(1, 3);
    let n = w.cands.len();
    let rows = n.div_ceil(cols) as u16;
    let scr = screen(app);
    // Same geometry as yazi: 1-cell side margins, one row above the status
    // bar, a padding row above and below the candidates.
    let h = (rows + 2).min(scr.height.saturating_sub(2));
    let rect = Rect::new(
        1.min(scr.width),
        scr.height.saturating_sub(h + 2),
        scr.width.saturating_sub(2),
        h,
    );
    Clear.render(rect, buf);
    buf.set_style(rect, th.s("which.mask"));
    let chunks =
        ratatui::layout::Layout::horizontal(vec![
            ratatui::layout::Constraint::Ratio(1, cols as u32);
            cols
        ])
        .split(rect);
    let sep = th.str("which.separator", "  ");
    let depth = w.keys.len();
    for (i, b) in w.cands.iter().enumerate() {
        let (row, col) = ((i / cols) as u16, i % cols);
        let y = rect.y + 1 + row;
        if y >= rect.y + rect.height.saturating_sub(1) {
            break;
        }
        let c = chunks[col];
        let key = b.on.get(depth).map(|k| k.to_string()).unwrap_or_default();
        let rest: String = b.on.iter().skip(depth + 1).map(|k| k.to_string()).collect();
        let kw = (width(&key) + width(&rest)) as u16;
        let mut x = c.x + 10u16.saturating_sub(kw);
        let end = c.x + c.width;
        let mut w2 = |s: &str, st: Style, x: &mut u16| {
            let n = put(buf, *x, y, s, end.saturating_sub(*x) as usize, st);
            *x += n as u16;
        };
        w2(&key, th.s("which.cand"), &mut x);
        w2(&rest, th.s("which.rest"), &mut x);
        w2(sep, th.s("which.separator_style"), &mut x);
        let desc = if b.desc.is_empty() {
            b.run_str()
        } else {
            b.desc.clone()
        };
        w2(&desc, th.s("which.desc"), &mut x);
    }
}

fn help(buf: &mut Buffer, app: &mut App) {
    let scr = screen(app);
    let rect = Rect::new(0, 0, scr.width, scr.height);
    let App { th, km, help, .. } = app;
    let th = &*th;
    let Some(h) = help.as_mut() else { return };
    let filter = h.filter.to_lowercase();
    let items: Vec<_> = km
        .bindings(h.layer)
        .iter()
        .filter(|b| {
            filter.is_empty()
                || b.desc.to_lowercase().contains(&filter)
                || b.run_str().to_lowercase().contains(&filter)
                || b.on_str().to_lowercase().contains(&filter)
        })
        .collect();
    Clear.render(rect, buf);
    let list_h = rect.height.saturating_sub(1) as usize;
    if !items.is_empty() {
        h.cursor = h.cursor.min(items.len() - 1);
    }
    if h.cursor < h.offset {
        h.offset = h.cursor;
    } else if h.cursor >= h.offset + list_h {
        h.offset = h.cursor + 1 - list_h;
    }
    // yazi's columns: key (20), command (30), description (rest).
    let (on_w, run_w) = (20usize, 30usize);
    for (row, (i, b)) in items
        .iter()
        .enumerate()
        .skip(h.offset)
        .take(list_h)
        .enumerate()
    {
        let y = row as u16;
        let hovered = i == h.cursor;
        if hovered {
            buf.set_style(Rect::new(0, y, rect.width, 1), th.s("help.hovered"));
        }
        put(buf, 0, y, &b.on_str(), on_w - 1, th.s("help.on"));
        let rs = b.run_str();
        put(
            buf,
            on_w as u16,
            y,
            &rs,
            (run_w - 1).min(rect.width.saturating_sub(on_w as u16) as usize),
            th.s("help.run"),
        );
        let x = (on_w + run_w) as u16;
        if x < rect.width {
            put(
                buf,
                x,
                y,
                &b.desc,
                (rect.width - x) as usize,
                th.s("help.desc"),
            );
        }
    }
    let fy = rect.height.saturating_sub(1);
    let footer = th.s("help.footer");
    buf.set_style(Rect::new(0, fy, rect.width, 1), footer);
    let text = if h.filter.is_empty() {
        format!("{}.help (Press `f` to filter)", h.layer.name())
    } else {
        format!("Filter: {}", h.filter)
    };
    put(buf, 0, fy, &text, rect.width as usize, footer);
}

fn tasks(buf: &mut Buffer, app: &App) {
    let Some(v) = &app.tasks_view else { return };
    let th = &app.th;
    let scr = screen(app);
    let w = (scr.width * 4).div_ceil(5).max(30);
    let h = (scr.height * 4).div_ceil(5).max(8);
    let rect = center(app, w, h);
    Clear.render(rect, buf);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(th.s("tasks.border"))
        .title(ratatui::text::Line::styled("Tasks", th.s("tasks.title")).centered());
    let inner = block.inner(rect);
    block.render(rect, buf);
    // yazi's `Tasks` component: pad (1, 1, 1, 3), three rows per task, a
    // 60/40 split for the gauge and the file counter.
    let area = Rect {
        x: inner.x + 3,
        y: inner.y + 1,
        width: inner.width.saturating_sub(4),
        height: inner.height.saturating_sub(2),
    };
    let left_w = area.width * 60 / 100;
    let gray = Style::default().fg(Color::Gray);
    let bottom = area.y + area.height;
    for (i, t) in app.tasks.iter().rev().enumerate() {
        let y = area.y + (i as u16) * 3;
        if y >= bottom {
            break;
        }
        let hovered = i == v.cursor;
        let st = if hovered {
            th.s("tasks.hovered")
        } else {
            Style::default()
        };
        let icon = match t.kind {
            crate::tasks::Kind::Copy => "  ",
            crate::tasks::Kind::Cut => "  ",
            crate::tasks::Kind::Delete => "  ",
            crate::tasks::Kind::Other => "  ",
        };
        let line = Rect::new(area.x, y, area.width, 1);
        if hovered {
            buf.set_style(line, st);
        }
        let n = put(buf, area.x, y, icon, area.width as usize, st);
        put(
            buf,
            area.x + n as u16,
            y,
            &printable(&t.title),
            (area.width as usize).saturating_sub(n),
            st,
        );
        // The left bar spanning the title and progress rows.
        let bx = area.x.saturating_sub(2);
        for dy in 0..2 {
            if y + dy < bottom {
                put(buf, bx, y + dy, "┃", 1, st);
            }
        }
        let py = y + 1;
        if py >= bottom {
            break;
        }
        let failed = matches!(t.state, crate::tasks::State::Failed(_));
        if t.kind != crate::tasks::Kind::Other {
            let pct = t.percent();
            let label = format!(
                "{pct:3}% - {} / {}",
                readable_size(t.bytes_done),
                readable_size(t.bytes_total)
            );
            let gs = if failed {
                th.s("status.progress_error")
            } else {
                th.s("status.progress_normal")
            };
            gauge(
                buf,
                Rect::new(area.x, py, left_w, 1),
                pct,
                &label,
                gs,
                th.s("status.progress_label"),
            );
            let count = format!("{}/{}", t.items_done, t.items_total);
            let cw = width(&count) as u16;
            let rx = area.x + area.width.saturating_sub(cw);
            put(buf, rx, py, &count, cw as usize, gray);
        } else {
            let text = match &t.state {
                crate::tasks::State::Running => "Running…",
                crate::tasks::State::Failed(_) => "Failed, press Enter to view log…",
                crate::tasks::State::Cancelled => "Cancelled",
                crate::tasks::State::Done => "Done",
            };
            put(buf, area.x, py, text, left_w as usize, gray);
        }
    }
}

/// ratatui's `Gauge` look: filled part in the style's fg as background,
/// centred label on top.
fn gauge(buf: &mut Buffer, a: Rect, pct: u16, label: &str, style: Style, label_style: Style) {
    if a.width == 0 {
        return;
    }
    let filled = (a.width as u32 * pct.min(100) as u32 / 100) as u16;
    let fill = Style::default()
        .bg(style.fg.unwrap_or(Color::Reset))
        .fg(style.bg.unwrap_or(Color::Reset));
    let empty = Style::default()
        .bg(style.bg.unwrap_or(Color::Reset))
        .fg(style.fg.unwrap_or(Color::Reset));
    for i in 0..a.width {
        let c = &mut buf[(a.x + i, a.y)];
        c.set_symbol(" ");
        c.set_style(if i < filled { fill } else { empty });
    }
    let lw = width(label) as u16;
    let lx = a.x + a.width.saturating_sub(lw) / 2;
    put(
        buf,
        lx,
        a.y,
        label,
        a.width.saturating_sub(lx - a.x) as usize,
        label_style,
    );
}

fn spot(buf: &mut Buffer, app: &App) {
    let Some(s) = &app.spot else { return };
    let th = &app.th;
    // yazi: `ui.Table(...):area(ui.Pos { "center", w = 60, h = 20 })`.
    let rect = center(app, 60, 20);
    Clear.render(rect, buf);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(th.s("spot.border"))
        .title(ratatui::text::Line::styled("Spot", th.s("spot.title")).centered());
    let inner = block.inner(rect);
    block.render(rect, buf);
    let hgt = inner.height as usize;
    let offset = s.cursor.saturating_sub(hgt.saturating_sub(1));
    let header = Style::default().fg(Color::Green);
    for (row, (i, (k, v))) in s.rows.iter().enumerate().skip(offset).take(hgt).enumerate() {
        let y = inner.y + row as u16;
        if v.is_empty() && !k.starts_with(' ') {
            put(buf, inner.x, y, k, inner.width as usize, header);
            continue;
        }
        put(buf, inner.x, y, k, 14, Style::default());
        let vx = inner.x + 14;
        let vw = (inner.x + inner.width).saturating_sub(vx);
        let st = if i == s.cursor {
            th.s("spot.tbl_col").patch(th.s("spot.tbl_cell"))
        } else {
            th.s("spot.tbl_col")
        };
        if i == s.cursor {
            buf.set_style(Rect::new(vx, y, vw, 1), st);
        }
        put(buf, vx, y, &printable(v), vw as usize, st);
    }
}

fn notifications(buf: &mut Buffer, app: &App) {
    if app.notes.is_empty() {
        return;
    }
    let th = &app.th;
    let scr = screen(app);
    let w = (scr.width / 3).clamp(24, 60).min(scr.width);
    let mut y = 1u16;
    for n in app.notes.iter().rev() {
        let inner_w = w.saturating_sub(2) as usize;
        let mut lines: Vec<String> = Vec::new();
        for l in n.body.lines() {
            let mut rest = l;
            while width(rest) > inner_w {
                let part = crate::util::truncate(rest, inner_w);
                if part.is_empty() {
                    break;
                }
                lines.push(part.to_string());
                rest = &rest[part.len()..];
            }
            lines.push(rest.to_string());
        }
        lines.truncate(6);
        let h = lines.len() as u16 + 2;
        if y + h > scr.height {
            break;
        }
        let rect = Rect::new(scr.width - w, y, w, h);
        let (tk, ik) = match n.level {
            Level::Info => ("notify.title_info", "notify.icon_info"),
            Level::Warn => ("notify.title_warn", "notify.icon_warn"),
            Level::Error => ("notify.title_error", "notify.icon_error"),
        };
        let ts = th.s(tk);
        Clear.render(rect, buf);
        let title = format!("{} {}", th.str(ik, ""), n.title);
        let block = popup_block(&title, ts, ts);
        let inner = block.inner(rect);
        block.render(rect, buf);
        for (i, l) in lines.iter().enumerate() {
            put(
                buf,
                inner.x,
                inner.y + i as u16,
                l,
                inner.width as usize,
                Style::default(),
            );
        }
        y += h;
    }
}

/// Width-aware helper used in tests.
#[allow(dead_code)]
pub fn text_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}
