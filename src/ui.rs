use std::io::{Stdout, Write};
use std::time::Duration;

use anyhow::Result;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
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

/// 光标闪烁周期。
pub const BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// 等待用户输入时可能收到的事件。
#[derive(Debug, Clone, Copy)]
pub enum UiEvent {
    Key(KeyEvent),
    Click(usize),
    ScrollUp,
    ScrollDown,
    /// 点击了“返回”按钮。
    Back,
    /// 鼠标悬停变化，需要重绘。
    Redraw,
}

pub struct Ui {
    out: Stdout,
    width: u16,
    height: u16,
    row: u16,
    /// 启动前的控制台代码页（Windows），退出时还原。
    codepage: Option<(u32, u32)>,
    /// 当前画面中可点击的行与对应列表下标。
    clicks: Vec<(u16, usize)>,
    /// 鼠标悬停的列表项（行、下标）。
    hover: Option<(u16, usize)>,
    /// “返回”按钮所在行。
    back_row: Option<u16>,
    /// 鼠标是否悬停在“返回”按钮上。
    hover_back: bool,
    /// 底部提示行绘制前的正文结束行，用于清除正文与底栏之间的残留。
    gap_start: Option<u16>,
    /// 由鼠标移动去抖逻辑暂存的事件，下次读取时优先处理。
    pending: Option<Event>,
}

impl Ui {
    pub fn enter() -> Result<Self> {
        // 统一使用 UTF-8 代码页，避免中文界面出现乱码。
        let codepage = switch_to_utf8();
        enable_raw_mode()?;
        let mut out = std::io::stdout();
        execute!(out, EnterAlternateScreen, Hide)?;
        // 部分终端不支持鼠标上报，失败时不影响启动。
        let _ = execute!(out, EnableMouseCapture);
        let (width, height) = terminal::size()?;
        Ok(Self {
            out,
            width,
            height,
            row: 0,
            codepage,
            clicks: Vec::new(),
            hover: None,
            back_row: None,
            hover_back: false,
            gap_start: None,
            pending: None,
        })
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn row(&self) -> u16 {
        self.row
    }

    pub fn begin(&mut self) -> Result<()> {
        let (width, height) = terminal::size()?;
        let resized = width != self.width || height != self.height;
        self.width = width;
        self.height = height;
        self.row = 0;
        self.clicks.clear();
        self.back_row = None;
        self.gap_start = None;
        if resized {
            queue!(self.out, Clear(ClearType::All), MoveTo(0, 0))?;
        } else {
            // 不做整屏清空，直接覆盖重绘，避免刷新时闪一下。
            queue!(self.out, MoveTo(0, 0))?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        // 清掉本帧没有用到的残留行：有底栏时只清正文与底栏之间的部分。
        let (start, end) = match self.gap_start.take() {
            Some(start) => (start.min(self.height), self.height.saturating_sub(1)),
            None => (self.row.min(self.height), self.height),
        };
        for row in start..end {
            queue!(self.out, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
        }
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
        self.print_line(
            &format!(
                "  NREL  ·  Minecraft Launcher  v{}",
                env!("CARGO_PKG_VERSION")
            ),
            Some(Color::Cyan),
            true,
            false,
        )?;
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
            let hovered = self.hover == Some((self.row, index));
            let text = format!(
                "  {} {}",
                if highlighted || hovered { "▶" } else { " " },
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
            } else if hovered {
                queue!(
                    self.out,
                    SetBackgroundColor(Color::DarkGrey),
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
            self.clicks.push((self.row, index));
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
            let hovered = self.hover == Some((self.row, index));
            let text = format!(
                "  {} {}",
                if highlighted || hovered { "▶" } else { " " },
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
            } else if hovered {
                queue!(
                    self.out,
                    SetBackgroundColor(Color::DarkGrey),
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
            self.clicks.push((self.row, index));
            self.row += 1;
            row_index += 1;
        }
        if row_index == 0 {
            self.dim_line("  （没有匹配项）")?;
        }
        Ok(())
    }

    /// 绘制底部提示行；`back` 为真时在右下角显示“返回”按钮（仅鼠标可点击）。
    pub fn footer(&mut self, hint: &str, back: bool) -> Result<()> {
        if self.height == 0 {
            return Ok(());
        }
        let y = self.height - 1;
        let button = " ‹ 返回 › ";
        let button_width = if back {
            UnicodeWidthStr::width(button)
        } else {
            0
        };
        let limit = (self.width as usize).saturating_sub(button_width + 1);
        queue!(
            self.out,
            MoveTo(0, y),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::DarkGrey),
            Print(truncate(hint, limit)),
            ResetColor
        )?;
        if back {
            let x = self.width.saturating_sub(button_width as u16 + 1);
            queue!(self.out, MoveTo(x, y))?;
            if self.hover_back {
                queue!(
                    self.out,
                    SetBackgroundColor(Color::DarkCyan),
                    SetForegroundColor(Color::White),
                    SetAttribute(Attribute::Bold)
                )?;
            } else {
                queue!(
                    self.out,
                    SetBackgroundColor(Color::DarkGrey),
                    SetForegroundColor(Color::Grey)
                )?;
            }
            queue!(
                self.out,
                Print(button),
                ResetColor,
                SetAttribute(Attribute::Reset)
            )?;
            self.back_row = Some(y);
        }
        // 记录正文结束行，供 flush 清除正文与底栏之间的残留。
        self.gap_start = Some(self.row);
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

    /// 阻塞等待一次有效输入（按键、点击列表项或滚轮）。
    pub fn wait(&mut self) -> Result<UiEvent> {
        loop {
            let event = match self.next_event()? {
                Some(event) => event,
                None => event::read()?,
            };
            if let Some(ui_event) = self.handle_event(event)? {
                return Ok(ui_event);
            }
        }
    }

    /// 在超时内等待一次有效输入；超时返回 `None`。
    pub fn poll_event(&mut self, timeout: Duration) -> Result<Option<UiEvent>> {
        if self.pending.is_none() && !event::poll(timeout)? {
            return Ok(None);
        }
        loop {
            let Some(event) = self.next_event()? else {
                return Ok(None);
            };
            if let Some(ui_event) = self.handle_event(event)? {
                return Ok(Some(ui_event));
            }
            if self.pending.is_none() && !event::poll(Duration::from_millis(0))? {
                return Ok(None);
            }
        }
    }

    /// 取下一个原始事件：优先返回去抖时暂存的事件，否则在有事件时立即读取。
    fn next_event(&mut self) -> Result<Option<Event>> {
        if let Some(event) = self.pending.take() {
            return Ok(Some(event));
        }
        if event::poll(Duration::ZERO)? {
            return Ok(Some(event::read()?));
        }
        Ok(None)
    }

    fn handle_event(&mut self, event: Event) -> Result<Option<UiEvent>> {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                // 键盘操作后取消鼠标悬停高亮。
                self.hover = None;
                self.hover_back = false;
                Ok(Some(UiEvent::Key(key)))
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if self.back_row == Some(mouse.row) {
                        return Ok(Some(UiEvent::Back));
                    }
                    Ok(self.click_index(mouse.row).map(UiEvent::Click))
                }
                MouseEventKind::Moved => {
                    let item = self
                        .clicks
                        .iter()
                        .find(|(click_row, _)| *click_row == mouse.row)
                        .copied();
                    let over_back = self.back_row == Some(mouse.row);
                    if item == self.hover && over_back == self.hover_back {
                        return Ok(None);
                    }
                    self.hover = item;
                    self.hover_back = over_back;
                    // 连续移动合并为一次重绘，避免整屏高频刷新造成闪烁。
                    self.settle_moves()?;
                    Ok(Some(UiEvent::Redraw))
                }
                MouseEventKind::ScrollUp => {
                    self.hover = None;
                    self.hover_back = false;
                    Ok(Some(UiEvent::ScrollUp))
                }
                MouseEventKind::ScrollDown => {
                    self.hover = None;
                    self.hover_back = false;
                    Ok(Some(UiEvent::ScrollDown))
                }
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    /// 鼠标持续移动时不断更新悬停目标，停顿约 50ms 后才返回，从而只重绘一次。
    fn settle_moves(&mut self) -> Result<()> {
        loop {
            if !event::poll(Duration::from_millis(50))? {
                return Ok(());
            }
            match event::read()? {
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved => {
                    self.hover = self
                        .clicks
                        .iter()
                        .find(|(click_row, _)| *click_row == mouse.row)
                        .copied();
                    self.hover_back = self.back_row == Some(mouse.row);
                }
                other => {
                    self.pending = Some(other);
                    return Ok(());
                }
            }
        }
    }

    fn click_index(&self, row: u16) -> Option<usize> {
        self.clicks
            .iter()
            .find(|(click_row, _)| *click_row == row)
            .map(|(_, index)| *index)
    }

    /// 原地重写一行，用于光标闪烁时局部刷新。
    pub fn rewrite_line(&mut self, row: u16, text: &str) -> Result<()> {
        if row >= self.height {
            return Ok(());
        }
        let text = truncate(text, self.width as usize);
        queue!(
            self.out,
            MoveTo(0, row),
            Clear(ClearType::CurrentLine),
            Print(text)
        )?;
        Ok(())
    }

    /// 底部命令输入行（最后一行）。
    pub fn prompt_line(&mut self, text: &str) -> Result<()> {
        if self.height == 0 {
            return Ok(());
        }
        let y = self.height - 1;
        queue!(
            self.out,
            MoveTo(0, y),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::Cyan),
            SetAttribute(Attribute::Bold),
            Print(truncate(text, self.width as usize)),
            ResetColor,
            SetAttribute(Attribute::Reset)
        )?;
        self.row = self.height;
        Ok(())
    }

    /// 命令模式的状态/错误提示行（倒数第二行）。
    pub fn status_line(&mut self, text: &str, error: bool) -> Result<()> {
        if self.height < 2 {
            return Ok(());
        }
        let y = self.height - 2;
        queue!(
            self.out,
            MoveTo(0, y),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(if error { Color::Red } else { Color::DarkGrey }),
            Print(truncate(text, self.width as usize)),
            ResetColor
        )?;
        Ok(())
    }

    /// 清除底部两行（命令模式退出时使用）。
    pub fn clear_bottom(&mut self) -> Result<()> {
        for y in [self.height.saturating_sub(2), self.height.saturating_sub(1)] {
            if y < self.height {
                queue!(self.out, MoveTo(0, y), Clear(ClearType::CurrentLine))?;
            }
        }
        Ok(())
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
        self.message_page(title, message, Color::Grey)
    }

    /// 信息页：正文用青色，与错误提示区分开。
    pub fn notice(&mut self, title: &str, message: &str) -> Result<()> {
        self.message_page(title, message, Color::Cyan)
    }

    fn message_page(&mut self, title: &str, message: &str, color: Color) -> Result<()> {
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
                self.print_line(&format!("  {line}"), Some(color), false, false)?;
            }
        }
        self.blank()?;
        self.footer("按任意键返回", false)?;
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
        let mut blink = true;
        let mut full = true;
        let mut dirty = true;
        let mut input_row = 0u16;

        loop {
            let text_max = (self.width as usize).saturating_sub(8).max(4);
            let (before, after) = visible_input(&chars, cursor, text_max);
            let before_width = UnicodeWidthStr::width(before.as_str());
            let after_width = UnicodeWidthStr::width(after.as_str());

            if full {
                self.header(title, hint)?;
                input_row = self.row;
                self.row = input_row + 1;
                self.blank()?;
                match &error {
                    Some(message) => {
                        self.print_line(&format!("  {message}"), Some(Color::Red), false, false)?
                    }
                    None => self.blank()?,
                }
                self.footer("Enter 确认    Esc 取消", false)?;
                full = false;
                dirty = true;
            }

            // 只在内容或光标相位变化时重绘，鼠标移动不重绘输入行。
            if dirty {
                let caret = if blink { "▏" } else { " " };
                let padding = text_max.saturating_sub(before_width + after_width);
                queue!(
                    self.out,
                    MoveTo(0, input_row),
                    Clear(ClearType::CurrentLine),
                    SetForegroundColor(Color::Cyan),
                    SetAttribute(Attribute::Bold),
                    MoveTo(2, input_row),
                    Print("› "),
                    ResetColor,
                    SetAttribute(Attribute::Reset),
                    Print(before),
                    Print(caret),
                    Print(after),
                    Print(" ".repeat(padding))
                )?;
                self.flush()?;
                dirty = false;
            }

            let Some(ui_event) = self.poll_event(BLINK_INTERVAL)? else {
                blink = !blink;
                dirty = true;
                continue;
            };
            // 鼠标移动只会带来重绘，不应重置光标相位，否则光标会快速闪烁。
            if matches!(ui_event, UiEvent::Key(_)) {
                blink = true;
            }
            let UiEvent::Key(key) = ui_event else {
                continue;
            };
            dirty = true;

            match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Enter => {
                    let text: String = chars.iter().collect();
                    let trimmed = text.trim();
                    let message = if !allow_empty && trimmed.is_empty() {
                        Some("不能为空".to_string())
                    } else {
                        validator.and_then(|validate| validate(trimmed).err())
                    };
                    match message {
                        Some(message) => {
                            error = Some(message);
                            full = true;
                        }
                        None => return Ok(Some(trimmed.to_string())),
                    }
                }
                KeyCode::Backspace => {
                    if cursor > 0 {
                        cursor -= 1;
                        chars.remove(cursor);
                        if error.take().is_some() {
                            full = true;
                        }
                    }
                }
                KeyCode::Delete => {
                    if cursor < chars.len() {
                        chars.remove(cursor);
                        if error.take().is_some() {
                            full = true;
                        }
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
                    if error.take().is_some() {
                        full = true;
                    }
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
            self.footer("↑↓/滚轮 选择    Enter/点击 确认    Esc 返回", true)?;
            self.flush()?;

            match self.wait()? {
                UiEvent::Key(key) => match key.code {
                    KeyCode::Up => selected = cycle_step(selected, items.len(), -1),
                    KeyCode::Down => selected = cycle_step(selected, items.len(), 1),
                    KeyCode::Enter => return Ok(Some(selected)),
                    KeyCode::Esc => return Ok(None),
                    _ => {}
                },
                UiEvent::Click(index) => return Ok(Some(index)),
                UiEvent::ScrollUp => selected = cycle_step(selected, items.len(), -1),
                UiEvent::ScrollDown => selected = cycle_step(selected, items.len(), 1),
                UiEvent::Back => return Ok(None),
                UiEvent::Redraw => {}
            }
        }
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.out, DisableMouseCapture, LeaveAlternateScreen, Show);
        let _ = self.out.flush();
        if let Some((input, output)) = self.codepage.take() {
            restore_codepage(input, output);
        }
    }
}

const UTF8_CODEPAGE: u32 = 65001;

/// 将控制台输入/输出代码页切换为 UTF-8，返回切换前的代码页用于还原。
#[cfg(windows)]
fn switch_to_utf8() -> Option<(u32, u32)> {
    use windows_sys::Win32::System::Console::{
        GetConsoleCP, GetConsoleOutputCP, SetConsoleCP, SetConsoleOutputCP,
    };

    unsafe {
        let input = GetConsoleCP();
        let output = GetConsoleOutputCP();
        if input == 0 || output == 0 {
            return None;
        }
        SetConsoleCP(UTF8_CODEPAGE);
        SetConsoleOutputCP(UTF8_CODEPAGE);
        Some((input, output))
    }
}

#[cfg(windows)]
fn restore_codepage(input: u32, output: u32) {
    use windows_sys::Win32::System::Console::{SetConsoleCP, SetConsoleOutputCP};

    unsafe {
        SetConsoleCP(input);
        SetConsoleOutputCP(output);
    }
}

#[cfg(not(windows))]
fn switch_to_utf8() -> Option<(u32, u32)> {
    None
}

#[cfg(not(windows))]
fn restore_codepage(_input: u32, _output: u32) {}

/// 列表上下移动，首尾循环。
pub fn cycle_step(selected: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let selected = selected.min(len - 1);
    if delta < 0 {
        if selected == 0 { len - 1 } else { selected - 1 }
    } else if selected + 1 >= len {
        0
    } else {
        selected + 1
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

/// 返回（光标前的可见文本、光标后的可见文本），并为光标预留一列。
fn visible_input(chars: &[char], cursor: usize, max_width: usize) -> (String, String) {
    let budget = max_width.saturating_sub(1);
    let limit = budget.saturating_sub(2);
    let mut start = 0usize;
    while start < cursor && width_of(&chars[start..cursor]) > limit {
        start += 1;
    }

    let mut width = width_of(&chars[start..cursor]);
    let mut end = cursor;
    while end < chars.len() {
        let char_width = char_width_of(Some(&chars[end]));
        if width + char_width > budget {
            break;
        }
        width += char_width;
        end += 1;
    }

    let before: String = chars[start..cursor].iter().collect();
    let after: String = chars[cursor..end].iter().collect();
    (before, after)
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
    // 直接使用 ANSI 序列着色，避免为一处彩色输出引入额外依赖。
    println!("  \x1b[36;1m›\x1b[0m \x1b[1m{title}\x1b[0m \x1b[2m{detail}\x1b[0m");
}

pub fn warn(message: impl std::fmt::Display) {
    println!("  \x1b[33;1m!\x1b[0m \x1b[33m{message}\x1b[0m");
}

