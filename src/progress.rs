use std::{
    io::{self, IsTerminal, Write},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    backend::{Backend, CrosstermBackend},
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::report::{Report, Status};

const FRAME_INTERVAL: Duration = Duration::from_millis(80);
const VIEWPORT_HEIGHT: u16 = 8;
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Setup,
    Schema,
    Impact,
    Build,
    Compare,
    Cleanup,
}

impl Phase {
    const ALL: [Self; 6] = [
        Self::Setup,
        Self::Schema,
        Self::Impact,
        Self::Build,
        Self::Compare,
        Self::Cleanup,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Setup => 0,
            Self::Schema => 1,
            Self::Impact => 2,
            Self::Build => 3,
            Self::Compare => 4,
            Self::Cleanup => 5,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Setup => "Verify local setup",
            Self::Schema => "Create temporary warehouse schemas",
            Self::Impact => "Map affected models",
            Self::Build => "Build selected models",
            Self::Compare => "Compare results with production",
            Self::Cleanup => "Remove temporary warehouse schemas",
        }
    }
}

#[derive(Debug, Clone)]
struct State {
    base: String,
    started: Instant,
    phase: Phase,
    failed_phase: Option<Phase>,
    affected: usize,
    selected: usize,
    built: usize,
    comparisons_done: usize,
    comparisons_total: usize,
    cleanup_done: usize,
    cleanup_total: usize,
    finished: bool,
    status: Option<Status>,
}

impl State {
    fn new(base: &str) -> Self {
        Self {
            base: base.to_owned(),
            started: Instant::now(),
            phase: Phase::Setup,
            failed_phase: None,
            affected: 0,
            selected: 0,
            built: 0,
            comparisons_done: 0,
            comparisons_total: 0,
            cleanup_done: 0,
            cleanup_total: 0,
            finished: false,
            status: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Reporter {
    state: Arc<Mutex<State>>,
}

impl Reporter {
    pub fn phase(&self, phase: Phase) {
        self.update(|state| state.phase = phase);
    }

    pub fn scope(&self, affected: usize, selected: usize) {
        self.update(|state| {
            state.affected = affected;
            state.selected = selected;
        });
    }

    pub fn built(&self, count: usize) {
        self.update(|state| state.built = count);
    }

    pub fn comparisons(&self, done: usize, total: usize) {
        self.update(|state| {
            state.comparisons_done = done;
            state.comparisons_total = total;
        });
    }

    pub fn cleanup(&self, done: usize, total: usize) {
        self.update(|state| {
            state.cleanup_done = done;
            state.cleanup_total = total;
        });
    }

    pub fn fail_current(&self) {
        self.update(|state| {
            state.failed_phase.get_or_insert(state.phase);
        });
    }

    fn update(&self, update: impl FnOnce(&mut State)) {
        if let Ok(mut state) = self.state.lock() {
            update(&mut state);
        }
    }
}

pub struct Display {
    reporter: Reporter,
    worker: Option<thread::JoinHandle<()>>,
}

impl Display {
    pub fn start(base: &str, json: bool, dry_run: bool) -> Option<Self> {
        if !enabled(json, dry_run) {
            return None;
        }
        let backend = CrosstermBackend::new(io::stderr());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_HEIGHT),
            },
        )
        .ok()?;
        let reporter = Reporter {
            state: Arc::new(Mutex::new(State::new(base))),
        };
        let state = Arc::clone(&reporter.state);
        let worker = thread::Builder::new()
            .name("embrasure-progress".to_owned())
            .spawn(move || render_loop(terminal, state))
            .ok()?;
        Some(Self {
            reporter,
            worker: Some(worker),
        })
    }

    pub fn reporter(&self) -> Reporter {
        self.reporter.clone()
    }

    pub fn finish(mut self, report: &Report) {
        self.reporter.update(|state| {
            state.finished = true;
            state.status = Some(report.status);
            state.cleanup_total = report.ci_schemas.len();
            state.cleanup_done = report
                .ci_schemas
                .iter()
                .filter(|schema| schema.cleaned_up)
                .count();
            if state.cleanup_done < state.cleanup_total {
                state.failed_phase = Some(Phase::Cleanup);
            } else if report.status == Status::ExecutionFailure && state.failed_phase.is_none() {
                state.failed_phase = Some(state.phase);
            }
        });
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        self.reporter.update(|state| state.finished = true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn enabled(json: bool, dry_run: bool) -> bool {
    enabled_for(
        json,
        dry_run,
        io::stdout().is_terminal() && io::stderr().is_terminal(),
        std::env::var_os("CI").is_some()
            || std::env::var_os("NO_COLOR").is_some()
            || std::env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb")),
    )
}

const fn enabled_for(
    json: bool,
    dry_run: bool,
    interactive_terminal: bool,
    plain_output: bool,
) -> bool {
    !json && !dry_run && interactive_terminal && !plain_output
}

type ProgressTerminal = Terminal<CrosstermBackend<io::Stderr>>;

fn render_loop(mut terminal: ProgressTerminal, state: Arc<Mutex<State>>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut frame = 0usize;
        loop {
            let snapshot = match state.lock() {
                Ok(state) => state.clone(),
                Err(_) => break,
            };
            if terminal
                .draw(|area| render(area, &snapshot, frame))
                .is_err()
            {
                break;
            }
            frame = frame.wrapping_add(1);
            if snapshot.finished {
                break;
            }
            thread::sleep(FRAME_INTERVAL);
        }
    }));
    let cleared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        clear_for_report(&mut terminal).is_ok()
    }))
    .unwrap_or(false);
    let _ = terminal.show_cursor();
    let _ = Backend::flush(terminal.backend_mut());
    drop(terminal);
    if !cleared {
        let mut stderr = io::stderr();
        for _ in 0..VIEWPORT_HEIGHT {
            let _ = stderr.write_all(b"\r\n");
        }
        let _ = stderr.flush();
    }
}

fn clear_for_report<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    terminal
        .draw(|frame| frame.set_cursor_position(frame.area().as_position()))
        .map(|_| ())
}

fn render(frame: &mut Frame<'_>, state: &State, tick: usize) {
    let render_area = Rect::new(
        frame.area().x,
        frame.area().y,
        frame.area().width,
        frame.area().height.min(VIEWPORT_HEIGHT),
    );
    frame.render_widget(Paragraph::new(lines(state, tick)), render_area);
}

fn lines(state: &State, frame: usize) -> Vec<Line<'static>> {
    let elapsed = format_elapsed(state.started.elapsed());
    let outcome = match state.status {
        Some(Status::Pass) => " · safe to continue",
        Some(Status::Findings) => " · findings found",
        Some(Status::Incomplete) => " · incomplete",
        Some(Status::ExecutionFailure) => " · stopped",
        None => "",
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "Embrasure review",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" · {} · {elapsed}{outcome}", state.base)),
    ])];
    for phase in Phase::ALL {
        let index = phase.index();
        let current = state.phase.index();
        let failed = state.failed_phase == Some(phase);
        let (symbol, style) = if failed {
            ("×".to_owned(), Style::default().fg(Color::Red))
        } else if index < current || (state.finished && index == current) {
            ("✓".to_owned(), Style::default().fg(Color::Green))
        } else if index == current {
            (
                SPINNER[frame % SPINNER.len()].to_string(),
                Style::default().fg(Color::Cyan),
            )
        } else {
            ("○".to_owned(), Style::default().fg(Color::DarkGray))
        };
        lines.push(step_line(&symbol, style, phase, state));
    }
    lines
}

fn step_line(symbol: &str, style: Style, phase: Phase, state: &State) -> Line<'static> {
    let detail = match phase {
        Phase::Impact if state.affected > 0 => {
            format!(
                "  {} affected · {} selected",
                state.affected, state.selected
            )
        }
        Phase::Build if state.selected > 0 => format!("  {} / {}", state.built, state.selected),
        Phase::Compare if state.comparisons_total > 0 => {
            format!("  {} / {}", state.comparisons_done, state.comparisons_total)
        }
        Phase::Cleanup if state.cleanup_total > 0 => {
            format!("  {} / {}", state.cleanup_done, state.cleanup_total)
        }
        _ => String::new(),
    };
    Line::from(vec![
        Span::styled(format!("{symbol} "), style),
        Span::raw(format!("{}{detail}", phase.label())),
    ])
}

fn format_elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn progress_uses_real_scope_and_counters() {
        let mut state = State::new("origin/main");
        state.phase = Phase::Compare;
        state.affected = 5;
        state.selected = 4;
        state.built = 4;
        state.comparisons_done = 3;
        state.comparisons_total = 5;
        let rendered = lines(&state, 0)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("5 affected · 4 selected"));
        assert!(rendered.contains("Build selected models  4 / 4"));
        assert!(rendered.contains("Compare results with production  3 / 5"));
    }

    #[test]
    fn review_board_renders_in_a_small_terminal() {
        let mut state = State::new("origin/main");
        state.phase = Phase::Build;
        state.affected = 5;
        state.selected = 4;
        state.built = 2;
        let backend = TestBackend::new(60, VIEWPORT_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state, 0)).unwrap();

        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("Embrasure review · origin/main"));
        assert!(content.contains("Build selected models  2 / 4"));
    }

    #[test]
    fn review_board_handles_a_tiny_terminal() {
        let state = State::new("origin/main");
        let backend = TestBackend::new(20, 3);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &state, 0)).unwrap();
    }

    #[test]
    fn final_output_replaces_the_review_board() {
        let state = State::new("origin/main");
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_HEIGHT),
            },
        )
        .unwrap();
        terminal.draw(|frame| render(frame, &state, 0)).unwrap();

        clear_for_report(&mut terminal).unwrap();
        terminal.backend_mut().assert_cursor_position((0, 0));
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| cell.symbol() == " ")
        );
    }

    #[test]
    fn progress_only_runs_in_an_interactive_terminal() {
        assert!(enabled_for(false, false, true, false));
        assert!(!enabled_for(true, false, true, false));
        assert!(!enabled_for(false, true, true, false));
        assert!(!enabled_for(false, false, false, false));
        assert!(!enabled_for(false, false, true, true));
    }

    #[test]
    fn elapsed_time_is_stable() {
        assert_eq!(format_elapsed(Duration::from_secs(131)), "02:11");
    }
}
