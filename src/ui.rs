use std::io::{Stdout, Write};
use std::time::Duration;

use anyhow::Result;
use console::style;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::terminal as terminal;
use crossterm::{execute, queue};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub type InputValidator<'a> = &'a dyn Fn(&str) -> Result<(), String>;

pub struct Ui {
    out: Stdout,
    width: u16,
    height: u16,
    row: u16,
}

impl Ui {
    pub fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut out = std::io::stdout();
        execute!(out, EnterAlternateScreen, Hide)?;
        let (width, height) = terminal::size()?;
        Ok(Self {
            out,
            width,
            height,
            row: 0,
        })
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn row(&self) -> u16 {
        self.row
    }

    pub fn begin(&mut self) -> Result<()> {
        let (width, height) = terminal::size()?;
        self.width = width;
        self.height = height;
        self.row = 0;
        queue!(self.out, Clear(ClearType::All), MoveTo(0, 0))?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }

    fn print_line(
        &mut self,
        text: &str,
        fg: Option<Color>,
        bold: bool,
        dim: bool,
    ) -> Result<()> {
        if self.row >= self.height {
            return Ok(());
        }
        let text = truncate(text, self.width as usize);
        queue!(
            self.out,
            MoveTo(0, self.row),
            Clear(ClearType::CurrentLine)
        )?;
        if let Some(fg) = fg {
            queue!(self.out, SetForegroundColor(fg))?;
        }
        if bold {
            queue!(self.out, SetAttribute(Attribute::Bold))?;
        }
        if dim {
            queue!(self.out, SetAttribute(Attribute::Dim))?;
        }
        queue!(
            self.out,
            Print(text),
            ResetColor,
            SetAttribute(Attribute::Reset)
        )?;
        self.row += 1;
        Ok(())
    }

    pub fn line(&mut self, text: &str) -> Result<()> {
        self.print_line(text, None, false, false)
    }

    pub fn dim_line(&mut self, text: &str) -> Result<()> {
        self.print_line(text, Some(Color::DarkGrey), false, false)
    }

    pub fn blank(&mut self) -> Result<()> {
        self.print_line("", None, false, false)
    }

    pub fn centered(&mut self, text: &str, fg: Color, bold: bool) -> Result<()> {
        let text_width = UnicodeWidthStr::width(text);
        let pad = (self.width as usize).saturating_sub(text_width) / 2;
        self.print_line(&format!("{}{}", " ".repeat(pad), text), Some(fg), bold, false)
    }

    pub fn header(&mut self, title: &str, subtitle: &str) -> Result<()> {
        self.begin()?;
        self.print_line("  NREL  ·  Minecraft Launcher", Some(Color::Cyan), true, false)?;
        self.blank()?;
        self.print_line(&format!("  {title}"), Some(Color::White), true, false)?;
        if !subtitle.is_empty() {
            self.print_line(&format!("  {subtitle}"), Some(Color::DarkGrey), false, false)?;
        }
        self.blank()?;
        Ok(())
    }

    pub fn card(&mut self, title: &str, rows: &[(&str, String)]) -> Result<()> {
        let inner_limit = (self.width as usize).saturating_sub(6).clamp(36, 76);
        let label_width = rows
            .iter()
            .map(|(label, _)| label.width())
            .max()
            .unwrap_or(0);
        let value_width = rows
            .iter()
            .map(|(_, value)| UnicodeWidthStr::width(value.as_str()))
            .max()
            .unwrap_or(0);
        let title_width = UnicodeWidthStr::width(title);
        let inner = (label_width + value_width + 4)
            .max(title_width + 4)
            .min(inner_limit);
        let value_width = inner.saturating_sub(label_width + 4);

        if title.is_empty() {
            self.border_line("╭", inner)?;
        } else {
            self.border_line(
                &format!("╭─ {} ", truncate(title, inner.saturating_sub(5))),
                inner,
            )?;
        }
        for (label, value) in rows {
            if self.row >= self.height {
                break;
            }
            let label = pad(label, label_width);
            let value = truncate(value, value_width);
            let value_display = UnicodeWidthStr::width(value.as_str());
            queue!(
                self.out,
                MoveTo(0, self.row),
                Clear(ClearType::CurrentLine),
                SetForegroundColor(Color::DarkCyan),
                Print("│"),
                ResetColor,
                Print(" "),
                SetAttribute(Attribute::Bold),
                Print(label),
                SetAttribute(Attribute::Reset),
                Print("  "),
                Print(value),
                Print(" ".repeat(value_width.saturating_sub(value_display))),
                Print(" "),
                SetForegroundColor(Color::DarkCyan),
                Print("│"),
                ResetColor
            )?;
            self.row += 1;
        }
        self.border_line("╰", inner)?;
        Ok(())
    }

    fn border_line(&mut self, left: &str, inner: usize) -> Result<()> {
        if self.row >= self.height {
            return Ok(());
        }
        let used = UnicodeWidthStr::width(left);
        let right = if left.starts_with('╭') { '╮' } else { '╯' };
        // 内容区宽度为 inner，加上左右边框后总宽度为 inner + 2。
        let fill = (inner + 1).saturating_sub(used);
        let text = format!("{left}{}{right}", "─".repeat(fill));
        queue!(
            self.out,
            MoveTo(0, self.row),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::DarkCyan),
            Print(text),
            ResetColor
        )?;
        self.row += 1;
        Ok(())
    }

    pub fn menu(&mut self, items: &[String], selected: usize) -> Result<()> {
        for (index, item) in items.iter().enumerate() {
            if self.row >= self.height {
                break;
            }
            let highlighted = index == selected;
            let text = format!(
                "  {} {}",
                if highlighted { "▶" } else { " " },
                item
            );
            let text = truncate(&text, self.width as usize);
            queue!(
                self.out,
                MoveTo(0, self.row),
                Clear(ClearType::CurrentLine)
            )?;
            if highlighted {
                queue!(
                    self.out,
                    SetBackgroundColor(Color::DarkCyan),
                    SetForegroundColor(Color::White),
                    SetAttribute(Attribute::Bold)
                )?;
            }
            let text_width = UnicodeWidthStr::width(text.as_str());
            queue!(
                self.out,
                Print(text.clone()),
                Print(" ".repeat((self.width as usize).saturating_sub(text_width))),
                ResetColor,
                SetAttribute(Attribute::Reset)
            )?;
            self.row += 1;
        }
        Ok(())
    }

    pub fn list(
        &mut self,
        items: &[String],
        selected: usize,
        start: usize,
        max_rows: usize,
    ) -> Result<()> {
        let end = (start + max_rows).min(items.len());
        let mut row_index = 0usize;
        for (index, item) in items
            .iter()
            .enumerate()
            .skip(start)
            .take(end.saturating_sub(start))
        {
            if self.row >= self.height {
                break;
            }
            let highlighted = index == selected;
            let text = format!(
                "  {} {}",
                if highlighted { "▶" } else { " " },
                item
            );
            let text = truncate(&text, self.width as usize);
            queue!(
                self.out,
                MoveTo(0, self.row),
                Clear(ClearType::CurrentLine)
            )?;
            if highlighted {
                queue!(
                    self.out,
                    SetBackgroundColor(Color::DarkCyan),
                    SetForegroundColor(Color::White),
                    SetAttribute(Attribute::Bold)
                )?;
            }
            let text_width = UnicodeWidthStr::width(text.as_str());
            queue!(
                self.out,
                Print(text),
                Print(" ".repeat((self.width as usize).saturating_sub(text_width))),
                ResetColor,
                SetAttribute(Attribute::Reset)
            )?;
            self.row += 1;
            row_index += 1;
        }
        if row_index == 0 {
            self.dim_line("  （没有匹配项）")?;
        }
        Ok(())
    }

    pub fn footer(&mut self, hint: &str) -> Result<()> {
        if self.height == 0 {
            return Ok(());
        }
        let y = self.height - 1;
        queue!(
            self.out,
            MoveTo(0, y),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::DarkGrey),
            Print(truncate(hint, self.width as usize)),
            ResetColor
        )?;
        if self.row < self.height {
            self.row = self.height;
        }
        Ok(())
    }

    pub fn key(&mut self) -> Result<KeyEvent> {
        loop {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => return Ok(key),
                _ => {}
            }
        }
    }

    pub fn wait_key(&mut self) -> Result<()> {
        self.key()?;
        Ok(())
    }

    pub fn poll_cancel(&mut self, timeout: Duration) -> Result<bool> {
        if !event::poll(timeout)? {
            return Ok(false);
        }
        if let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            let is_esc = key.code == KeyCode::Esc;
            let is_ctrl_c = key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL);
            return Ok(is_esc || is_ctrl_c);
        }
        Ok(false)
    }

    pub fn begin_work(
        &mut self,
        title: impl std::fmt::Display,
        detail: impl std::fmt::Display,
    ) -> Result<()> {
        self.begin()?;
        self.print_line(
            &format!("  NREL  ›  {title}"),
            Some(Color::Cyan),
            true,
            false,
        )?;
        self.print_line(&format!("  {detail}"), Some(Color::DarkGrey), false, false)?;
        self.blank()?;
        self.flush()
    }

    pub fn work_line(&mut self, text: &str) -> Result<()> {
        self.print_line(&format!("  {text}"), None, false, false)
    }

    /// 全屏错误/提示页，按键后返回。
    pub fn alert(&mut self, title: &str, message: &str) -> Result<()> {
        self.begin()?;
        self.print_line(
            "  NREL  ·  Minecraft Launcher",
            Some(Color::Cyan),
            true,
            false,
        )?;
        self.blank()?;
        self.print_line(&format!("  {title}"), Some(Color::White), true, false)?;
        self.blank()?;
        let limit = (self.width as usize).saturating_sub(4).max(10);
        for paragraph in message.split('\n') {
            if paragraph.trim().is_empty() {
                self.blank()?;
                continue;
            }
            for line in wrap(paragraph, limit) {
                self.print_line(&format!("  {line}"), Some(Color::Red), false, false)?;
            }
        }
        self.blank()?;
        self.footer("按任意键返回")?;
        self.flush()?;
        self.wait_key()
    }

    pub fn input(
        &mut self,
        title: &str,
        hint: &str,
        initial: &str,
        allow_empty: bool,
        validator: Option<InputValidator<'_>>,
    ) -> Result<Option<String>> {
        let mut chars: Vec<char> = initial.chars().collect();
        let mut cursor = chars.len();
        let mut error: Option<String> = None;
        execute!(self.out, Show)?;

        loop {
            let text_max = (self.width as usize).saturating_sub(8).max(4);
            let (shown, caret) = visible_input(&chars, cursor, text_max);

            self.header(title, hint)?;
            let input_row = self.row;
            let shown = truncate(&shown, text_max);
            let shown_width = UnicodeWidthStr::width(shown.as_str());
            queue!(
                self.out,
                MoveTo(2, input_row),
                SetForegroundColor(Color::Cyan),
                SetAttribute(Attribute::Bold),
                Print("› "),
                ResetColor,
                SetAttribute(Attribute::Reset),
                Print(shown),
                Print(" ".repeat(text_max.saturating_sub(shown_width))),
                MoveTo(4 + caret.min(text_max) as u16, input_row),
                Show
            )?;
            self.row = input_row + 1;
            self.blank()?;
            match &error {
                Some(message) => {
                    self.print_line(&format!("  {message}"), Some(Color::Red), false, false)?
                }
                None => self.blank()?,
            }
            self.footer("Enter 确认    Esc 取消")?;
            self.flush()?;

            let key = self.key()?;
            match key.code {
                KeyCode::Esc => {
                    execute!(self.out, Hide)?;
                    return Ok(None);
                }
                KeyCode::Enter => {
                    let text: String = chars.iter().collect();
                    let trimmed = text.trim();
                    if !allow_empty && trimmed.is_empty() {
                        error = Some("不能为空".to_string());
                        continue;
                    }
                    if let Some(validate) = validator
                        && let Err(message) = validate(trimmed)
                    {
                        error = Some(message);
                        continue;
                    }
                    execute!(self.out, Hide)?;
                    return Ok(Some(trimmed.to_string()));
                }
                KeyCode::Backspace => {
                    if cursor > 0 {
                        cursor -= 1;
                        chars.remove(cursor);
                        error = None;
                    }
                }
                KeyCode::Delete => {
                    if cursor < chars.len() {
                        chars.remove(cursor);
                        error = None;
                    }
                }
                KeyCode::Left => cursor = cursor.saturating_sub(1),
                KeyCode::Right => cursor = (cursor + 1).min(chars.len()),
                KeyCode::Home => cursor = 0,
                KeyCode::End => cursor = chars.len(),
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    chars.insert(cursor, c);
                    cursor += 1;
                    error = None;
                }
                _ => {}
            }
        }
    }

    pub fn pick(
        &mut self,
        title: &str,
        subtitle: &str,
        items: &[String],
        selected: usize,
    ) -> Result<Option<usize>> {
        if items.is_empty() {
            return Ok(None);
        }
        let mut selected = selected.min(items.len() - 1);
        let mut start = 0usize;
        loop {
            self.header(title, subtitle)?;
            let max_rows = (self.height.saturating_sub(self.row + 1) as usize).max(1);
            if selected >= start + max_rows {
                start = selected + 1 - max_rows;
            }
            if selected < start {
                start = selected;
            }
            self.list(items, selected, start, max_rows)?;
            self.footer("↑↓ 选择    Enter 确认    Esc 返回")?;
            self.flush()?;

            let key = self.key()?;
            match key.code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => selected = (selected + 1).min(items.len() - 1),
                KeyCode::Enter => return Ok(Some(selected)),
                KeyCode::Esc => return Ok(None),
                _ => {}
            }
        }
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.out, LeaveAlternateScreen, Show);
        let _ = self.out.flush();
    }
}

fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width + 1 > max {
            break;
        }
        out.push(ch);
        width += char_width;
    }
    out.push('…');
    out
}

fn pad(text: &str, width: usize) -> String {
    let text_width = UnicodeWidthStr::width(text);
    format!("{text}{}", " ".repeat(width.saturating_sub(text_width)))
}

/// 返回（可见文本、光标在文本中的显示宽度）。
fn visible_input(chars: &[char], cursor: usize, max_width: usize) -> (String, usize) {
    let limit = max_width.saturating_sub(2);
    let mut start = 0usize;
    while start < cursor && width_of(&chars[start..cursor]) > limit {
        start += 1;
    }

    let caret = width_of(&chars[start..cursor]);

    let mut shown = String::new();
    let mut width = 0usize;
    for ch in chars.iter().skip(start) {
        let char_width = char_width_of(Some(ch));
        if width + char_width > max_width {
            break;
        }
        shown.push(*ch);
        width += char_width;
    }
    (shown, caret)
}

fn width_of(chars: &[char]) -> usize {
    chars.iter().map(|ch| char_width_of(Some(ch))).sum()
}

/// 按显示宽度换行，用于错误信息。
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in text.chars() {
        if ch == '\r' {
            continue;
        }
        let char_width = char_width_of(Some(&ch));
        if current_width + char_width > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += char_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn char_width_of(ch: Option<&char>) -> usize {
    ch.and_then(|c| UnicodeWidthChar::width(*c)).unwrap_or(0)
}

pub fn step(title: impl std::fmt::Display, detail: impl std::fmt::Display) {
    println!(
        "  {} {} {}",
        style("›").cyan().bold(),
        style(title).bold(),
        style(detail).dim()
    );
}

pub fn ok(message: impl std::fmt::Display) {
    println!("  {} {}", style("✓").green().bold(), message);
}

pub fn warn(message: impl std::fmt::Display) {
    println!("  {} {}", style("!").yellow().bold(), style(message).yellow());
}

