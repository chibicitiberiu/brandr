//! Screen building blocks for the netflash wizard: a frame (title bar, heading,
//! footer), a filterable list with a details pane, and a dialog with buttons.
//!
//! Everything is drawn with plain box-drawing characters and 8 colours so it
//! looks the same on a Linux VGA text console (80x25) as in a terminal emulator.

use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame, Terminal,
    layout::{Constraint, Direction, Layout, Rect},
    prelude::Backend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

pub const STEPS: u8 = 4;

const INDENT: u16 = 3;

fn title_style() -> Style {
    Style::default().fg(Color::White).bg(Color::Blue)
}
fn heading_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}
fn section_style() -> Style {
    Style::default().fg(Color::Yellow)
}
fn selected_style() -> Style {
    Style::default().fg(Color::Black).bg(Color::Cyan)
}
fn disabled_style() -> Style {
    Style::default().fg(Color::DarkGray)
}
fn dim_style() -> Style {
    Style::default().fg(Color::Gray)
}
pub fn warn_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}
pub fn danger_style() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}
pub fn label_style() -> Style {
    Style::default().fg(Color::Gray)
}
pub fn value_style() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

/// Header and footer common to every screen. Returns the body area.
pub struct Chrome<'a> {
    /// e.g. "Step 2 of 4  Target"; empty for screens outside the steps
    pub step: String,
    pub heading: &'a str,
    /// text shown right-aligned on the heading line (e.g. the filter)
    pub heading_right: String,
    pub keys: &'a str,
}

impl Chrome<'_> {
    pub fn step(n: u8, name: &str) -> String {
        format!("Step {n} of {STEPS}  {name}")
    }

    fn draw(&self, f: &mut Frame) -> Rect {
        let area = f.size();
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // title bar
                Constraint::Length(1), // space
                Constraint::Length(1), // heading
                Constraint::Length(1), // space
                Constraint::Min(3),    // body
                Constraint::Length(1), // footer
            ])
            .split(area);

        let width = area.width as usize;
        let left = " netflash";
        let right = format!("{} ", self.step);
        let pad = width.saturating_sub(left.len() + right.chars().count());
        f.render_widget(
            Paragraph::new(format!("{left}{}{right}", " ".repeat(pad))).style(title_style()),
            rows[0],
        );

        let head = indent(rows[2]);
        let hr = self.heading_right.chars().count();
        let hpad = (head.width as usize).saturating_sub(self.heading.chars().count() + hr + 1);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(self.heading.to_string(), heading_style()),
                Span::raw(" ".repeat(hpad)),
                Span::styled(self.heading_right.clone(), warn_style()),
            ])),
            head,
        );

        f.render_widget(
            Paragraph::new(format!(" {}", self.keys)).style(title_style()),
            rows[5],
        );
        indent(rows[4])
    }
}

fn indent(r: Rect) -> Rect {
    Rect {
        x: r.x + INDENT,
        width: r.width.saturating_sub(INDENT * 2),
        ..r
    }
}

/// Column layout of a list row.
#[derive(Clone, Copy)]
pub enum Col {
    /// takes the remaining width (only one per row)
    Flex,
    Left(u16),
    Right(u16),
}

pub enum Row {
    Section(String),
    Item {
        cols: Vec<String>,
        /// greyed out and not selectable
        disabled: bool,
        /// shown in the details pane while selected
        detail: Vec<Line<'static>>,
        /// what the filter matches against
        search: String,
    },
}

pub struct ListSpec<'a> {
    pub chrome: Chrome<'a>,
    pub layout: Vec<Col>,
    pub rows: Vec<Row>,
    pub filterable: bool,
    /// index into `rows` to start on (must be an enabled item)
    pub initial: Option<usize>,
    /// lines of the details pane (0 = none)
    pub detail_height: u16,
}

pub enum ListResult {
    Selected(usize),
    Back,
}

/// Show a list and return the chosen row index (into `spec.rows`).
pub fn list_screen<B: Backend>(
    term: &mut Terminal<B>,
    mut spec: ListSpec,
) -> io::Result<ListResult> {
    let mut filter = String::new();
    let mut selected = spec.initial;
    let mut offset = 0usize;

    loop {
        // rows visible under the current filter; sections only if they have a match
        let visible = visible_rows(&spec.rows, &filter);
        let items: Vec<usize> = visible
            .iter()
            .flatten()
            .copied()
            .filter(|&i| selectable(&spec.rows[i]))
            .collect();
        if !selected.is_some_and(|s| items.contains(&s)) {
            selected = items.first().copied();
        }
        spec.chrome.heading_right = if filter.is_empty() {
            String::new()
        } else {
            format!("filter: {filter}_")
        };

        term.draw(|f| {
            let body = spec.chrome.draw(f);
            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(if spec.detail_height > 0 {
                        spec.detail_height + 2
                    } else {
                        0
                    }),
                ])
                .split(body);
            let list_area = parts[0];
            // when the list scrolls, its last line is kept for the "N more" hint
            let scrolls = visible.len() > list_area.height as usize;
            let height = list_area.height as usize - usize::from(scrolls);

            // keep the selection in view
            let sel_pos = selected
                .and_then(|s| visible.iter().position(|&i| i == Some(s)))
                .unwrap_or(0);
            if sel_pos < offset {
                offset = sel_pos.saturating_sub(1);
            } else if sel_pos >= offset + height {
                offset = sel_pos + 1 - height;
            }
            offset = offset.min(visible.len().saturating_sub(height));

            let lines: Vec<Line> = visible
                .iter()
                .skip(offset)
                .take(height)
                .map(|&i| match i {
                    Some(i) => render_row(
                        &spec.rows[i],
                        &spec.layout,
                        list_area.width,
                        Some(i) == selected,
                    ),
                    None => Line::raw(""),
                })
                .collect();
            f.render_widget(Paragraph::new(lines), list_area);

            if visible.is_empty() {
                f.render_widget(
                    Paragraph::new("Nothing matches the filter.").style(dim_style()),
                    list_area,
                );
            }
            if scrolls {
                let more = Rect {
                    y: list_area.y + list_area.height - 1,
                    height: 1,
                    ..list_area
                };
                let below = visible.len() - offset - height;
                let text = match (offset, below) {
                    (0, n) => format!("{n} more below"),
                    (n, 0) => format!("{n} more above"),
                    (a, b) => format!("{a} above, {b} below"),
                };
                let x = more.width.saturating_sub(text.len() as u16);
                f.render_widget(
                    Paragraph::new(text).style(dim_style()),
                    Rect {
                        x: more.x + x,
                        width: more.width - x,
                        ..more
                    },
                );
            }

            if spec.detail_height > 0 {
                let detail = selected
                    .and_then(|s| match &spec.rows[s] {
                        Row::Item { detail, .. } => Some(detail.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                f.render_widget(
                    Paragraph::new(detail).wrap(Wrap { trim: false }).block(
                        Block::default()
                            .borders(Borders::TOP)
                            .border_style(dim_style()),
                    ),
                    Rect {
                        y: parts[1].y + 1,
                        height: parts[1].height.saturating_sub(1),
                        ..parts[1]
                    },
                );
            }
        })?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let pos = selected.and_then(|s| items.iter().position(|&i| i == s));
        let jump = |delta: isize| -> Option<usize> {
            let p = pos.map_or(0, |p| {
                (p as isize + delta).clamp(0, items.len() as isize - 1) as usize
            });
            items.get(p).copied()
        };
        match key.code {
            KeyCode::Up => selected = jump(-1).or(selected),
            KeyCode::Down => selected = jump(1).or(selected),
            KeyCode::PageUp => selected = jump(-10).or(selected),
            KeyCode::PageDown => selected = jump(10).or(selected),
            KeyCode::Home => selected = items.first().copied(),
            KeyCode::End => selected = items.last().copied(),
            KeyCode::Enter => {
                if let Some(s) = selected {
                    return Ok(ListResult::Selected(s));
                }
            }
            KeyCode::Esc if !filter.is_empty() => filter.clear(),
            KeyCode::Esc => return Ok(ListResult::Back),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(ListResult::Back);
            }
            KeyCode::Backspace if spec.filterable => {
                filter.pop();
            }
            KeyCode::Char(c) if spec.filterable && !c.is_control() => filter.push(c),
            _ => {}
        }
    }
}

fn selectable(row: &Row) -> bool {
    matches!(
        row,
        Row::Item {
            disabled: false,
            ..
        }
    )
}

/// Row indices shown under the current filter; `None` is a blank spacer line.
/// A section header only shows when something under it matches.
fn visible_rows(rows: &[Row], filter: &str) -> Vec<Option<usize>> {
    let needle = filter.to_lowercase();
    let matches = |r: &Row| match r {
        Row::Item { search, .. } => needle.is_empty() || search.to_lowercase().contains(&needle),
        Row::Section(_) => false,
    };
    let mut out = Vec::new();
    let mut pending_section: Option<usize> = None;
    for (i, r) in rows.iter().enumerate() {
        match r {
            Row::Section(_) => pending_section = Some(i),
            _ if matches(r) => {
                if let Some(s) = pending_section.take() {
                    if !out.is_empty() {
                        out.push(None);
                    }
                    out.push(Some(s));
                }
                out.push(Some(i));
            }
            _ => {}
        }
    }
    out
}

fn render_row<'a>(row: &'a Row, layout: &[Col], width: u16, selected: bool) -> Line<'a> {
    match row {
        Row::Section(title) => Line::styled(fit(title, width as usize, false), section_style()),
        Row::Item { cols, disabled, .. } => {
            let width = width as usize;
            let fixed: usize = layout
                .iter()
                .map(|c| match c {
                    Col::Left(w) | Col::Right(w) => *w as usize + 2,
                    Col::Flex => 0,
                })
                .sum();
            let flex = width.saturating_sub(fixed + 2).max(8);
            let mut text = String::from(if selected { "> " } else { "  " });
            // a single cell spans the whole row (e.g. action entries under a table)
            let spanning = [Col::Flex];
            let layout = if cols.len() == 1 {
                &spanning[..]
            } else {
                layout
            };
            let flex = if cols.len() == 1 {
                width.saturating_sub(2)
            } else {
                flex
            };
            for (col, value) in layout.iter().zip(cols) {
                let cell = match col {
                    Col::Flex => fit(value, flex, false),
                    Col::Left(w) => fit(value, *w as usize, false),
                    Col::Right(w) => fit(value, *w as usize, true),
                };
                text.push_str(&cell);
                if !matches!(col, Col::Flex) || layout.len() > 1 {
                    text.push_str("  ");
                }
            }
            let text = fit(text.trim_end(), width, false);
            let style = if selected {
                selected_style()
            } else if *disabled {
                disabled_style()
            } else {
                Style::default()
            };
            Line::styled(text, style)
        }
    }
}

/// Pad or truncate to exactly `w` characters.
fn fit(s: &str, w: usize, right: bool) -> String {
    let n = s.chars().count();
    if n > w {
        let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
        t.push('~');
        t
    } else if right {
        format!("{}{s}", " ".repeat(w - n))
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}

/// A screen of text with a row of buttons. Returns the chosen button index, or
/// None on Esc. `focus` is the initially highlighted button.
pub fn dialog<B: Backend>(
    term: &mut Terminal<B>,
    chrome: Chrome,
    body: Vec<Line<'static>>,
    buttons: &[&str],
    focus: usize,
) -> io::Result<Option<usize>> {
    let mut focus = focus.min(buttons.len().saturating_sub(1));
    loop {
        term.draw(|f| {
            let area = chrome.draw(f);
            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(2)])
                .split(area);
            f.render_widget(
                Paragraph::new(body.clone()).wrap(Wrap { trim: false }),
                parts[0],
            );
            let mut spans = Vec::new();
            for (i, b) in buttons.iter().enumerate() {
                let style = if i == focus {
                    selected_style()
                } else {
                    dim_style()
                };
                spans.push(Span::styled(format!("[ {b} ]"), style));
                spans.push(Span::raw("   "));
            }
            f.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect {
                    y: parts[1].y + 1,
                    height: 1,
                    ..parts[1]
                },
            );
        })?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Left | KeyCode::BackTab => focus = focus.saturating_sub(1),
            KeyCode::Right | KeyCode::Tab => focus = (focus + 1).min(buttons.len() - 1),
            KeyCode::Enter => return Ok(Some(focus)),
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            _ => {}
        }
    }
}

/// Draw a text-only screen once (for screens that refresh on a timer).
pub fn draw_static(f: &mut Frame, chrome: &Chrome, body: &[Line<'static>]) {
    let area = chrome.draw(f);
    f.render_widget(
        Paragraph::new(body.to_vec()).wrap(Wrap { trim: false }),
        area,
    );
}

/// "Label   value" line for overview screens.
pub fn field(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        // at least two spaces between label and value, even for long labels
        Span::styled(format!("{:<12}", format!("{label}  ")), label_style()),
        Span::styled(value, value_style()),
    ])
}

/// Continuation line under a field (indented to the value column).
pub fn field_more(value: String) -> Line<'static> {
    Line::from(vec![
        Span::raw(" ".repeat(12)),
        Span::styled(value, dim_style()),
    ])
}
