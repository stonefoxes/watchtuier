//! Two-panel ratatui rendering.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::app::{App, Focus};

/// Render the whole app into the provided frame.
pub fn draw(f: &mut Frame, app: &App) {
    let total = f.area();

    // Body (two stacked rows) + status bar at the bottom.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(1)])
        .split(total);
    let body = chunks[0];
    let status_area = chunks[1];

    let panels = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(body);
    let top = panels[0];
    let bottom = panels[1];

    draw_usb_panel(f, app, top);
    draw_net_panel(f, app, bottom);
    draw_status(f, app, status_area);
}

fn draw_usb_panel(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Usb;
    let title = format!(" USB serial devices  ({}) ", app.usb_devices.len());
    let block = make_block(&title, focused);

    let header_cells = ["Dev", "Type", "Chip", "VID:PID", "Serial"];
    let header = Row::new(
        header_cells
            .iter()
            .map(|h| Cell::new(*h).style(Style::default().add_modifier(Modifier::BOLD))),
    )
    .height(1);

    let rows = app.usb_devices.iter().enumerate().map(|(i, d)| {
        let is_latest = app.is_latest_usb(&d.devnode);
        let is_selected = focused && app.usb_cursor == i;
        let style = cell_style(is_selected, is_latest);
        let cells = vec![
            Cell::from(d.devnode.clone()),
            Cell::from(d.kind.as_str().to_string()),
            Cell::from(d.chip.clone()),
            Cell::from(if d.vendor_id.is_empty() && d.product_id.is_empty() {
                String::new()
            } else {
                format!("{}:{}", d.vendor_id, d.product_id)
            }),
            Cell::from(d.serial.clone()),
        ];
        Row::new(cells).style(style)
    });

    let widths = [
        Constraint::Length(14),
        Constraint::Length(8),
        Constraint::Min(20),
        Constraint::Length(11),
        Constraint::Min(8),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(row_highlight_style(focused));
    f.render_widget(table, area);
}

fn draw_net_panel(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Net;
    let sel = app
        .selected_network()
        .map(|n| n.label.clone())
        .unwrap_or_default();
    let title = format!(" Network neighbours  [{}] ", sel);
    let block = make_block(&title, focused);

    let header_cells = ["IP", "Hostname", "Iface", "State"];
    let header = Row::new(
        header_cells
            .iter()
            .map(|h| Cell::new(*h).style(Style::default().add_modifier(Modifier::BOLD))),
    )
    .height(1);

    let visible = app.visible_neighbor_rows();
    let rows = visible
        .iter()
        .enumerate()
        .map(|(i, (ip, hostname, iface, state))| {
            let is_latest = app.is_latest_neighbor(*ip);
            let is_selected = focused && app.net_cursor == i;
            let style = cell_style(is_selected, is_latest);
            let hostname_cell = match hostname {
                Some(h) if !h.is_empty() => Cell::from(h.clone()),
                _ => Cell::from("—".to_string()).style(Style::default().fg(Color::DarkGray)),
            };
            let cells: Vec<Cell> = vec![
                Cell::from(ip.to_string()),
                hostname_cell,
                Cell::from(iface.clone()),
                Cell::from(state.clone()),
            ];
            Row::new(cells).style(style)
        });

    let widths = [
        Constraint::Length(16),
        Constraint::Min(20),
        Constraint::Length(8),
        Constraint::Min(10),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(row_highlight_style(focused));
    f.render_widget(table, area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let focus_tag = match app.focus {
        Focus::Usb => "USB",
        Focus::Net => "NET",
    };
    let mut spans = vec![
        Span::styled(
            format!("[{focus_tag}] "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(&app.status),
    ];
    if app.usb_devices.is_empty() && visible_neighbor_count(app) == 0 {
        spans.push(Span::styled(
            "  (no devices yet — plug in a USB serial adapter or wait for neighbour discovery)",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let para = Paragraph::new(Line::from(spans)).alignment(Alignment::Left);
    f.render_widget(para, area);
}

fn visible_neighbor_count(app: &App) -> usize {
    app.visible_neighbor_rows().len()
}

fn make_block(title: &str, focused: bool) -> Block<'_> {
    let border_color = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title_alignment(Alignment::Left)
        .title_style(Style::default().add_modifier(Modifier::BOLD))
        .title(title)
}

fn cell_style(selected: bool, latest: bool) -> Style {
    let mut s = Style::default();
    if latest {
        s = s.fg(Color::Yellow).add_modifier(Modifier::BOLD);
    }
    if selected {
        s = s.bg(Color::DarkGray);
    }
    s
}

fn row_highlight_style(focused: bool) -> Style {
    if focused {
        Style::default().bg(Color::DarkGray)
    } else {
        Style::default()
    }
}
