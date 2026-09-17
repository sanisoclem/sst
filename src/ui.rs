use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize as _};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Padding, Paragraph, Row,
    Table, TableState, Wrap,
};

use tui_textarea::TextArea;

use crate::app::{App, Asking, Connect, Focus, Modal, Picker, Stage};
use crate::config::Encryption;
use crate::db::ResultSet;

const SIDEBAR_WIDTH: u16 = 34;
const NARROWEST_GRID: u16 = 40;

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const NULL: Color = Color::Magenta;

const HELP: &[(&str, &str)] = &[
    ("Ctrl-Enter", "run the query (Ctrl-R works too)"),
    ("Tab", "completions (Ctrl-Space also); Tab again accepts"),
    ("Ctrl-E", "edit the query in $EDITOR"),
    (
        "Tab / Esc / i",
        "move between the queries, the editor and the results",
    ),
    ("Ctrl-B", "show or hide the query list"),
    ("Ctrl-S / S", "save the current query to the list"),
    ("h j k l", "move around the result grid"),
    ("0 / $", "first / last column"),
    ("[ / ]", "previous / next result set"),
    ("Enter", "show the whole cell"),
    ("x", "show the whole row, one column per line"),
    ("V", "send the cell to $PAGER"),
    ("e", "export the whole result set to .csv or .xlsx"),
    ("d", "switch database"),
    ("A", "switch account; then e to edit one, d to forget it"),
    ("?", "this help"),
    ("q / Ctrl-C", "quit"),
];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let editor_height = (app.query.lines().len() as u16 + 2).clamp(4, 14);
    let outer = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let show_sidebar = app.sidebar && outer[1].width > SIDEBAR_WIDTH + NARROWEST_GRID;
    let body = Layout::horizontal([
        Constraint::Length(if show_sidebar { SIDEBAR_WIDTH } else { 0 }),
        Constraint::Min(20),
    ])
    .split(outer[1]);

    let panes =
        Layout::vertical([Constraint::Length(editor_height), Constraint::Min(3)]).split(body[1]);

    header(frame, app, outer[0]);
    if show_sidebar {
        library(frame, app, body[0]);
    }
    editor(frame, app, panes[0]);
    grid(frame, app, panes[1]);
    status(frame, app, outer[2]);

    if app.focus == Focus::Query {
        completion(frame, app, panes[0]);
    }

    match &mut app.modal {
        Modal::None => {}
        Modal::Help => help(frame),
        Modal::Cell {
            title,
            body,
            scroll,
        } => cell(frame, title, body, *scroll),
        Modal::Confirm { prompt, .. } => confirm(frame, prompt),
        Modal::Picker(picker) => chooser(frame, picker),
        Modal::Connect(form) => connect(frame, form),
        Modal::Prompt { purpose, field } => ask(frame, *purpose, field),
        Modal::DeviceCode(prompt) => sign_in(frame, prompt),
    }
}

fn ask(frame: &mut Frame, purpose: Asking, field: &mut TextArea<'static>) {
    let (title, hint) = match purpose {
        Asking::ExportPath => (
            " Export to ",
            " .csv or .xlsx · writes every row · Enter write · Esc cancel ",
        ),
        Asking::QueryName => (
            " Save query as ",
            " kept for this server and database · Enter save · Esc cancel ",
        ),
    };

    let area = centered(frame.area(), 76, 5);
    frame.render_widget(Clear, area);
    field.set_block(
        popup(title).title_bottom(
            Line::from(hint)
                .right_aligned()
                .style(Style::new().fg(MUTED)),
        ),
    );
    field.set_cursor_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_widget(&*field, area);
}

fn library(frame: &mut Frame, app: &App, area: Rect) {
    let section = |title: &str, count: usize| {
        ListItem::new(Line::from(Span::styled(
            format!("{title} ({count})"),
            Style::new().fg(MUTED).italic(),
        )))
    };

    let mut items: Vec<ListItem> = vec![section("SAVED", app.library.saved.len())];
    for entry in &app.library.saved {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("★ ", Style::new().fg(Color::Yellow)),
            Span::raw(entry.label.clone()),
        ])));
    }
    items.push(section("RECENT", app.library.recent.len()));
    for entry in &app.library.recent {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("  {}", entry.label),
            Style::new().fg(Color::Gray),
        ))));
    }

    let highlighted = past_the_section_headings(app.library_selected, app.library.saved.len());
    let mut state =
        ListState::default().with_selected((!app.library.is_empty()).then_some(highlighted));

    let focused = app.focus == Focus::Library;
    frame.render_stateful_widget(
        List::new(items)
            .block(
                pane(" Queries ", focused).title_bottom(
                    Line::from(match focused {
                        true => " Enter load · s save · d delete ",
                        false => " ⇧Tab ",
                    })
                    .right_aligned()
                    .style(Style::new().fg(MUTED)),
                ),
            )
            .highlight_style(Style::new().bg(ACCENT).fg(Color::Black)),
        area,
        &mut state,
    );
}

fn past_the_section_headings(selected: usize, saved: usize) -> usize {
    match selected < saved {
        true => selected + 1,
        false => selected + 2,
    }
}

fn header(frame: &mut Frame, app: &App, area: Rect) {
    let connection = app.connection.clone().unwrap_or("not connected".into());
    let database = app.database.clone().unwrap_or_default();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" sst ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw(" "),
            Span::styled(connection, Style::new().fg(MUTED)),
            Span::raw("  "),
            Span::styled(database, Style::new().bold()),
            Span::raw("  "),
            Span::styled(
                match app.tables.len() {
                    0 => String::new(),
                    count => format!("{count} tables"),
                },
                Style::new().fg(MUTED),
            ),
            Span::raw("  "),
            Span::styled(
                transport_warning(app.encryption),
                Style::new().fg(Color::Red).bold(),
            ),
        ])),
        area,
    );
}

fn transport_warning(encryption: Encryption) -> &'static str {
    match encryption {
        Encryption::Off => "NO TLS",
        Encryption::LoginOnly => "TLS: login only",
        _ => "",
    }
}

fn editor(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Query;
    app.query.set_block(
        pane(" Query ", focused).title_bottom(
            Line::from(match focused {
                true => " ^Enter run · Tab complete · ^E $EDITOR · Esc results ",
                false => " i to edit ",
            })
            .right_aligned()
            .style(Style::new().fg(MUTED)),
        ),
    );
    app.query.set_cursor_style(match focused {
        true => Style::new().add_modifier(Modifier::REVERSED),
        false => Style::new(),
    });
    frame.render_widget(&app.query, area);
}

fn completion(frame: &mut Frame, app: &App, editor: Rect) {
    let Some(state) = &app.completion else { return };

    let (line, _) = app.query.cursor();
    let width = 46u16.min(frame.area().width.saturating_sub(4));
    let height = (state.items.len() as u16 + 2).min(10);
    let x = (editor.x + 1 + state.start as u16).min(frame.area().width.saturating_sub(width + 1));
    let below = editor.y + 1 + line as u16 + 1;
    let y = match below + height <= frame.area().height {
        true => below,
        false => below.saturating_sub(height + 1),
    };

    let items: Vec<ListItem> = state
        .items
        .iter()
        .map(|candidate| {
            ListItem::new(Line::from(vec![
                Span::raw(candidate.label.clone()),
                Span::styled(
                    format!("  {}", candidate.detail),
                    Style::new().fg(MUTED).italic(),
                ),
            ]))
        })
        .collect();

    let area = Rect {
        x,
        y,
        width,
        height,
    };
    let mut list_state = ListState::default().with_selected(Some(state.selected));
    frame.render_widget(Clear, area);
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(ACCENT)),
            )
            .highlight_style(Style::new().bg(ACCENT).fg(Color::Black)),
        area,
        &mut list_state,
    );
}

fn grid(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Results;
    let Some(widths) = app.result().map(column_widths) else {
        frame.render_widget(nothing_yet(focused), area);
        return;
    };

    let shown = scrolled_columns(app, &widths, area.width.saturating_sub(2));
    let title = grid_title(app, &shown);
    let (row, column) = (app.row, app.column);
    let Some(result) = app.result() else { return };

    let mut state = TableState::default().with_selected(Some(row));
    frame.render_stateful_widget(
        Table::new(
            grid_rows(result, &shown, row, column),
            widths[shown.clone()].to_vec(),
        )
        .header(grid_header(result, &shown))
        .block(pane(&title, focused))
        .column_spacing(1),
        area,
        &mut state,
    );
}

fn nothing_yet(focused: bool) -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(
        "  write a query and press Ctrl-Enter",
        Style::new().fg(MUTED),
    )))
    .block(pane(" Results ", focused))
}

fn scrolled_columns(app: &mut App, widths: &[u16], available: u16) -> Range<usize> {
    if app.column < app.column_offset {
        app.column_offset = app.column;
    }
    while app.column >= app.column_offset + fitting(widths, app.column_offset, available) {
        app.column_offset += 1;
    }
    let first = app.column_offset.min(widths.len().saturating_sub(1));
    first..(first + fitting(widths, first, available)).min(widths.len())
}

fn grid_header<'a>(result: &'a ResultSet, shown: &Range<usize>) -> Row<'a> {
    Row::new(
        result.columns[shown.clone()]
            .iter()
            .map(|name| Cell::from(Span::styled(name.clone(), Style::new().bold())))
            .collect::<Vec<_>>(),
    )
    .style(Style::new().fg(ACCENT))
}

fn grid_rows<'a>(
    result: &'a ResultSet,
    shown: &Range<usize>,
    row: usize,
    column: usize,
) -> Vec<Row<'a>> {
    result
        .rows
        .iter()
        .enumerate()
        .map(|(index, cells)| {
            let selected = index == row;
            let window = shown.start.min(cells.len())..shown.end.min(cells.len());
            Row::new(
                cells[window]
                    .iter()
                    .enumerate()
                    .map(|(offset, cell)| {
                        let mut style = match cell {
                            Some(_) => Style::new(),
                            None => Style::new().fg(NULL).italic(),
                        };
                        if selected && shown.start + offset == column {
                            style = style.bg(ACCENT).fg(Color::Black);
                        } else if selected {
                            style = style.bg(Color::Rgb(40, 44, 52));
                        }
                        Cell::from(cell.as_deref().unwrap_or("NULL")).style(style)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn grid_title(app: &App, shown: &Range<usize>) -> String {
    let Some(result) = app.result() else {
        return String::new();
    };
    let tabs = match app.results.len() {
        0 | 1 => String::new(),
        count => format!(" [{}/{count}]", app.active_result + 1),
    };
    let span = match result.columns.len() > shown.len() {
        true => format!(
            "  cols {}–{} of {}",
            shown.start + 1,
            shown.end,
            result.columns.len()
        ),
        false => String::new(),
    };
    let selected = match (result.columns.get(app.column), result.types.get(app.column)) {
        (Some(name), Some(kind)) => format!("  ·  {name} {kind}"),
        _ => String::new(),
    };
    format!(
        " Results{tabs}  {} rows{}{span}{selected} ",
        result.rows.len(),
        match result.truncated {
            true => " shown",
            false => "",
        }
    )
}

fn column_widths(result: &ResultSet) -> Vec<u16> {
    result
        .columns
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let longest = result
                .rows
                .iter()
                .filter_map(|row| row.get(index))
                .map(|cell| cell.as_deref().unwrap_or("NULL").chars().count())
                .max()
                .unwrap_or(0);
            (longest.max(name.chars().count()) as u16 + 2).clamp(6, 40)
        })
        .collect()
}

fn fitting(widths: &[u16], from: usize, available: u16) -> usize {
    let mut used = 0u16;
    let mut count = 0usize;
    for width in widths.iter().skip(from) {
        let needed = match count {
            0 => *width,
            _ => width + 1,
        };
        if used + needed > available {
            break;
        }
        used += needed;
        count += 1;
    }
    count.max(1)
}

fn status(frame: &mut Frame, app: &App, area: Rect) {
    const HINTS: &str = "^Enter run · Tab complete · e export · ^B queries · A account · ? help ";
    let (marker, style) = match (app.busy, app.failed) {
        (true, _) => ("⋯ ", Style::new().fg(Color::Yellow)),
        (_, true) => ("✗ ", Style::new().fg(Color::Red)),
        _ => ("", Style::new().fg(MUTED)),
    };

    let hints = HINTS.chars().count() as u16;
    let room = area
        .width
        .saturating_sub(marker.len() as u16 + app.status.chars().count() as u16 + 2);
    let columns = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(if room >= hints { hints } else { 0 }),
    ])
    .split(area);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(format!("{marker}{}", app.status), style),
        ])),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(HINTS, Style::new().fg(MUTED))))
            .alignment(Alignment::Right),
        columns[1],
    );
}

fn connect(frame: &mut Frame, form: &mut Connect) {
    let rows = form.visible();
    let area = centered(frame.area(), 74, rows.len() as u16 * 3 + 4);
    frame.render_widget(Clear, area);
    frame.render_widget(
        popup(match (form.editing.is_some(), form.azure) {
            (true, true) => " Edit account · Azure (device code) ",
            (true, false) => " Edit account · SQL auth ",
            (false, true) => " Connect · Azure (device code) ",
            (false, false) => " Connect · SQL auth ",
        })
        .title_bottom(
            Line::from(" ^T auth · Tab next · Enter connect · Esc back ")
                .right_aligned()
                .style(Style::new().fg(MUTED)),
        ),
        area,
    );

    let inner = area.inner(Margin::new(2, 1));
    let mut constraints: Vec<Constraint> = rows.iter().map(|_| Constraint::Length(3)).collect();
    constraints.push(Constraint::Length(1));
    constraints.push(Constraint::Min(0));
    let slots = Layout::vertical(constraints).split(inner);

    for (slot, which) in rows.iter().enumerate() {
        let focused = *which == form.active;
        let field = form.field_mut(*which);
        field.set_block(pane(which.label(), focused));
        field.set_cursor_style(match focused {
            true => Style::new().add_modifier(Modifier::REVERSED),
            false => Style::new(),
        });
        frame.render_widget(&*field, slots[slot]);
    }

    let certificate = match form.trust_certificate {
        true => "trusted",
        false => "verified",
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("^L encryption ", Style::new().fg(MUTED)),
            Span::styled(
                form.encrypt.label(),
                match form.encrypt {
                    Encryption::Off => Style::new().fg(Color::Red).bold(),
                    Encryption::Required => Style::new().fg(ACCENT).bold(),
                    _ => Style::new().fg(Color::Yellow).bold(),
                },
            ),
            Span::styled("   ^K certificate ", Style::new().fg(MUTED)),
            Span::styled(
                certificate,
                match form.trust_certificate {
                    true => Style::new().fg(Color::Yellow).bold(),
                    false => Style::new().fg(ACCENT).bold(),
                },
            ),
        ])),
        slots[rows.len()],
    );
}

fn sign_in(frame: &mut Frame, prompt: &crate::auth::Prompt) {
    let lines = match prompt {
        crate::auth::Prompt::DeviceCode {
            user_code,
            verification_uri,
        } => vec![
            Line::from(""),
            Line::from("Open this page and enter the code:").alignment(Alignment::Center),
            Line::from(Span::styled(
                verification_uri.clone(),
                Style::new().fg(ACCENT).underlined(),
            ))
            .alignment(Alignment::Center),
            Line::from(""),
            Line::from(Span::styled(
                user_code.clone(),
                Style::new().fg(Color::Black).bg(ACCENT).bold(),
            ))
            .alignment(Alignment::Center),
        ],
        crate::auth::Prompt::Browser { url } => vec![
            Line::from(""),
            Line::from("Finish signing in in the browser.").alignment(Alignment::Center),
            Line::from(Span::styled(
                "If it did not open, paste this:",
                Style::new().fg(MUTED),
            ))
            .alignment(Alignment::Center),
            Line::from(""),
            Line::from(Span::styled(url.clone(), Style::new().fg(ACCENT))),
        ],
    };

    let area = centered(frame.area(), 96, lines.len() as u16 + 4);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            popup(" Azure sign-in ").title_bottom(
                Line::from(" waiting… · Esc cancel ")
                    .right_aligned()
                    .style(Style::new().fg(MUTED)),
            ),
        ),
        area,
    );
}

fn help(frame: &mut Frame) {
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(keys, what)| {
            Line::from(vec![
                Span::styled(format!("{keys:>12}"), Style::new().fg(ACCENT).bold()),
                Span::raw("  "),
                Span::raw(*what),
            ])
        })
        .collect();
    let area = centered(frame.area(), 74, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(popup(" Keys ")), area);
}

fn cell(frame: &mut Frame, title: &str, body: &str, scroll: usize) {
    let area = centered(frame.area(), 96, 30);
    let heading = format!(" {title} ");
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0))
            .block(
                popup(&heading).title_bottom(
                    Line::from(" j/k scroll · q close ")
                        .right_aligned()
                        .style(Style::new().fg(MUTED)),
                ),
            ),
        area,
    );
}

fn confirm(frame: &mut Frame, prompt: &str) {
    let area = centered(frame.area(), prompt.chars().count() as u16 + 8, 5);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(prompt).alignment(Alignment::Center),
            Line::from(Span::styled("y / n", Style::new().fg(MUTED))).alignment(Alignment::Center),
        ])
        .block(popup(" Confirm ").border_style(Style::new().fg(Color::Red))),
        area,
    );
}

fn chooser(frame: &mut Frame, picker: &Picker) {
    let width = picker
        .choices
        .iter()
        .map(|choice| choice.label.chars().count())
        .max()
        .unwrap_or(0);
    let items: Vec<ListItem> = picker
        .choices
        .iter()
        .map(|choice| {
            let mut spans = vec![Span::raw(choice.label.clone())];
            if !choice.detail.is_empty() {
                let gap = width - choice.label.chars().count();
                spans.push(Span::styled(
                    format!("{:gap$}  {}", "", choice.detail),
                    Style::new().fg(MUTED),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let height = (picker.choices.len() as u16 + 2).clamp(5, 24);
    let area = centered(frame.area(), 64, height);
    let heading = format!(" {} ", picker.title);
    let block = popup(&heading).title_bottom(
        Line::from(match picker.stage {
            Stage::Account { .. } => " Enter connect · e edit · d forget ",
            Stage::Database => " Enter open ",
        })
        .right_aligned()
        .style(Style::new().fg(MUTED)),
    );

    let mut state = ListState::default().with_selected(Some(picker.selected));
    frame.render_widget(Clear, area);
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_style(Style::new().bg(ACCENT).fg(Color::Black))
            .highlight_symbol("▌"),
        area,
        &mut state,
    );
}

fn pane<'a>(title: &'a str, focused: bool) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { ACCENT } else { MUTED }))
        .title(Line::from(title).style(Style::new().bold()))
}

fn popup<'a>(title: &'a str) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .padding(Padding::horizontal(1))
        .title(Line::from(title).style(Style::new().bold()))
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(4));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::fitting;

    #[test]
    fn fits_as_many_columns_as_the_width_allows() {
        let widths = vec![10, 10, 10, 10, 10];
        assert_eq!(fitting(&widths, 0, 32), 3);
        assert_eq!(fitting(&widths, 0, 42), 3);
        assert_eq!(fitting(&widths, 0, 43), 4);
    }

    #[test]
    fn counts_from_the_scroll_offset() {
        let widths = vec![30, 10, 10, 10];
        assert_eq!(
            fitting(&widths, 0, 32),
            1,
            "the wide first column crowds it out"
        );
        assert_eq!(fitting(&widths, 1, 32), 3, "past it, three fit");
    }

    #[test]
    fn always_draws_at_least_one_column() {
        assert_eq!(fitting(&[200], 0, 40), 1);
        assert_eq!(fitting(&[200, 10], 0, 40), 1);
    }

    #[test]
    fn an_offset_past_the_end_does_not_panic() {
        assert_eq!(fitting(&[10, 10], 5, 100), 1);
        assert_eq!(fitting(&[], 0, 100), 1);
    }
}
