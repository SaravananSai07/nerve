use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme::Theme;
use crate::detect::claude::{attach_command, job_id};
use crate::state::session::{Session, SessionKind};
use crate::util::sanitize::Sanitised;
use crate::util::text::wrap_words;

const LABEL_COLS: usize = 13;

/// What `c` copies from the details view: the command that reaches a
/// background job, or the folder for sessions that have a tab or app.
pub(crate) fn copy_target(session: &Session) -> (&'static str, String) {
    match session.kind {
        SessionKind::Background => ("attach command", attach_command(session.id.as_str(), &session.claude_root)),
        _ => ("folder path", Sanitised::new(session.cwd.to_string_lossy()).as_str().to_string()),
    }
}

/// Everything nerve knows about one session. The card only has room for
/// a summary, which left parked background jobs unidentifiable: no repo,
/// no way in, and a waiting reason clipped to the card width.
fn rows(session: &Session) -> Vec<(&'static str, String)> {
    let path = |p: &std::path::Path| Sanitised::new(p.to_string_lossy()).as_str().to_string();
    // The card's timer counts from when nerve first saw the state, which
    // for a long-parked job is misleading; "last active" is the real age.
    let mut rows = vec![("state", session.state().label())];
    if let Some(why) = &session.waiting_for {
        rows.push(("waiting for", why.as_str().to_string()));
    }
    if let Some(age) = session.activity_age_secs {
        rows.push(("last active", crate::util::text::format_age(age)));
    }
    // High up, so a short terminal clips the ids rather than the way in.
    let reach = match session.kind {
        SessionKind::Background => attach_command(session.id.as_str(), &session.claude_root),
        SessionKind::Desktop => "Enter brings the Claude app forward".to_string(),
        SessionKind::Terminal => "Enter switches to its tab".to_string(),
    };
    rows.push(("reach it", reach));
    rows.push(("folder", path(&session.cwd)));
    if let Some(branch) = &session.branch {
        rows.push(("branch", branch.as_str().to_string()));
    }
    rows.push(("runs in", session.kind.location(session.tty.as_ref()).to_string()));
    if let Some(pid) = session.pid {
        rows.push(("pid", pid.to_string()));
    }
    rows.push(("session id", session.id.as_str().to_string()));
    if session.kind == SessionKind::Background {
        rows.push(("job id", job_id(session.id.as_str()).to_string()));
    }
    rows.push(("claude data", path(&session.claude_root)));
    if let Some(log) = &session.jsonl_path {
        rows.push(("transcript", path(log)));
    }
    rows
}

pub(crate) fn render(frame: &mut Frame, theme: &Theme, session: &Session) {
    let frame_area = frame.area();
    let width = 84u16.min(frame_area.width.saturating_sub(2));
    let inner_width = width.saturating_sub(2).max(1) as usize;

    let value_cols = inner_width.saturating_sub(LABEL_COLS).max(8);
    let rows: Vec<_> = rows(session)
        .into_iter()
        .map(|(label, value)| (label, wrap_words(&value, value_cols)))
        .collect();
    let body_rows: usize = rows.iter().map(|(_, v)| v.len()).sum();
    let height = (body_rows as u16 + 4).min(frame_area.height.saturating_sub(2));
    let area = super::centered(frame_area, width, height);

    frame.render_widget(Clear, area);
    let title = crate::util::text::truncate_width(session.name().as_str(), inner_width.saturating_sub(2));
    let block = Block::default()
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.processing));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for (label, chunks) in rows {
        let value_style = match label {
            "waiting for" => Style::default().fg(theme.waiting),
            "reach it" => Style::default().fg(theme.processing).add_modifier(Modifier::BOLD),
            _ => Style::default().fg(theme.text),
        };
        // Continuation lines hang under the value, not the label.
        for (i, chunk) in chunks.into_iter().enumerate() {
            let gutter = if i == 0 { label } else { "" };
            lines.push(Line::from(vec![
                Span::styled(format!(" {gutter:<w$}", w = LABEL_COLS - 1), Style::default().fg(theme.muted)),
                Span::styled(chunk, value_style),
            ]));
        }
    }
    // The key hints get their own rows so a tall body can't push them off.
    let [body_area, footer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(inner);
    frame.render_widget(Paragraph::new(lines), body_area);

    let (copy_label, _) = copy_target(session);
    let footer = vec![Line::raw(""), Line::from(vec![
        Span::styled(" [Enter]", Style::default().fg(theme.processing).add_modifier(Modifier::BOLD)),
        Span::styled(" go to session  ", Style::default().fg(theme.muted)),
        Span::styled("[c]", Style::default().fg(theme.processing).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" copy {copy_label}  "), Style::default().fg(theme.muted)),
        Span::styled("[Esc]", Style::default().fg(theme.processing).add_modifier(Modifier::BOLD)),
        Span::styled(" close", Style::default().fg(theme.muted)),
    ])];
    frame.render_widget(Paragraph::new(footer), footer_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::session::{DiscoverySnapshot, SessionState};

    // Regression: a parked background job showed only "background ⎇ main"
    // and a clipped question, with no repo and no way to reach it.
    #[test]
    fn background_details_name_the_repo_and_the_way_in() {
        let mut snap = DiscoverySnapshot::for_test("78240abc-845e-4", SessionState::WaitingForInput);
        snap.name = Sanitised::new("Analyze repository");
        snap.kind = SessionKind::Background;
        snap.cwd = "/Users/me/ape".into();
        snap.claude_root = "/Users/me/.claude".into();
        snap.waiting_for = Some(Sanitised::new("reload app, open mic, Connect, ask a question"));
        let s = Session::from_snapshot(snap);
        assert_eq!(s.project().as_ref().map(Sanitised::as_str), Some("ape"));
        let rows = rows(&s);
        let get = |k| rows.iter().find(|(l, _)| *l == k).map(|(_, v)| v.as_str());
        assert_eq!(get("folder"), Some("/Users/me/ape"));
        assert_eq!(get("job id"), Some("78240abc"));
        assert_eq!(get("waiting for"), Some("reload app, open mic, Connect, ask a question"));
        assert_eq!(get("reach it"), Some("CLAUDE_CONFIG_DIR='/Users/me/.claude' claude attach 78240abc"));
        assert_eq!(copy_target(&s).1, get("reach it").unwrap());
    }
}
