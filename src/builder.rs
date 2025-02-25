use crate::{
    execute::{CodeExecuter, ExecutionHandle, ExecutionState, ProcessStatus},
    markdown::{
        elements::{
            Code, CodeLanguage, Highlight, HighlightGroup, ListItem, ListItemType, MarkdownElement, ParagraphElement,
            SourcePosition, StyledText, Table, TableRow, Text,
        },
        text::{WeightedLine, WeightedText},
    },
    presentation::{
        AsRenderOperations, ChunkMutator, MarginProperties, PreformattedLine, Presentation, PresentationMetadata,
        PresentationThemeMetadata, RenderOnDemand, RenderOnDemandState, RenderOperation, Slide, SlideChunk,
    },
    render::{
        highlighting::{CodeHighlighter, LanguageHighlighter, StyledTokens},
        properties::WindowSize,
    },
    resource::{LoadImageError, Resources},
    style::{Colors, TextStyle},
    theme::{Alignment, AuthorPositioning, ElementType, FooterStyle, LoadThemeError, Margin, PresentationTheme},
};
use itertools::Itertools;
use serde::Deserialize;
use std::{borrow::Cow, cell::RefCell, fmt::Display, iter, mem, path::PathBuf, rc::Rc, str::FromStr};
use syntect::highlighting::Style;
use unicode_width::UnicodeWidthStr;

// TODO: move to a theme config.
static DEFAULT_BOTTOM_SLIDE_MARGIN: u16 = 3;

pub(crate) struct PresentationBuilderOptions {
    pub(crate) allow_mutations: bool,
}

impl Default for PresentationBuilderOptions {
    fn default() -> Self {
        Self { allow_mutations: true }
    }
}

/// Builds a presentation.
///
/// This type transforms [MarkdownElement]s and turns them into a presentation, which is made up of
/// render operations.
pub(crate) struct PresentationBuilder<'a> {
    slide_chunks: Vec<SlideChunk>,
    chunk_operations: Vec<RenderOperation>,
    chunk_mutators: Vec<Box<dyn ChunkMutator>>,
    slides: Vec<Slide>,
    highlighter: CodeHighlighter,
    theme: Cow<'a, PresentationTheme>,
    resources: &'a mut Resources,
    slide_state: SlideState,
    footer_context: Rc<RefCell<FooterContext>>,
    options: PresentationBuilderOptions,
}

impl<'a> PresentationBuilder<'a> {
    /// Construct a new builder.
    pub(crate) fn new(
        default_highlighter: CodeHighlighter,
        default_theme: &'a PresentationTheme,
        resources: &'a mut Resources,
        options: PresentationBuilderOptions,
    ) -> Self {
        Self {
            slide_chunks: Vec::new(),
            chunk_operations: Vec::new(),
            chunk_mutators: Vec::new(),
            slides: Vec::new(),
            highlighter: default_highlighter,
            theme: Cow::Borrowed(default_theme),
            resources,
            slide_state: Default::default(),
            footer_context: Default::default(),
            options,
        }
    }

    /// Build a presentation.
    pub(crate) fn build(mut self, elements: Vec<MarkdownElement>) -> Result<Presentation, BuildError> {
        if let Some(MarkdownElement::FrontMatter(contents)) = elements.first() {
            self.process_front_matter(contents)?;
        }
        self.set_code_theme()?;

        if self.chunk_operations.is_empty() {
            self.push_slide_prelude();
        }
        for element in elements {
            self.slide_state.ignore_element_line_break = false;
            self.process_element(element)?;
            self.validate_last_operation()?;
            if !self.slide_state.ignore_element_line_break {
                self.push_line_break();
            }
        }
        if !self.chunk_operations.is_empty() || !self.slide_chunks.is_empty() {
            self.terminate_slide();
        }
        self.footer_context.borrow_mut().total_slides = self.slides.len();

        let presentation = Presentation::new(self.slides);
        Ok(presentation)
    }

    fn validate_last_operation(&mut self) -> Result<(), BuildError> {
        if !self.slide_state.needs_enter_column {
            return Ok(());
        }
        let Some(last) = self.chunk_operations.last() else {
            return Ok(());
        };
        if matches!(last, RenderOperation::InitColumnLayout { .. }) {
            return Ok(());
        }
        self.slide_state.needs_enter_column = false;
        let last_valid = matches!(last, RenderOperation::EnterColumn { .. } | RenderOperation::ExitLayout);
        if last_valid { Ok(()) } else { Err(BuildError::NotInsideColumn) }
    }

    fn push_slide_prelude(&mut self) {
        let colors = self.theme.default_style.colors.clone();
        self.chunk_operations.extend([
            RenderOperation::SetColors(colors),
            RenderOperation::ClearScreen,
            RenderOperation::ApplyMargin(MarginProperties {
                horizontal_margin: self.theme.default_style.margin.clone().unwrap_or_default(),
                bottom_slide_margin: DEFAULT_BOTTOM_SLIDE_MARGIN,
            }),
        ]);
        self.push_line_break();
    }

    fn process_element(&mut self, element: MarkdownElement) -> Result<(), BuildError> {
        let should_clear_last = !matches!(element, MarkdownElement::List(_) | MarkdownElement::Comment { .. });
        match element {
            // This one is processed before everything else as it affects how the rest of the
            // elements is rendered.
            MarkdownElement::FrontMatter(_) => self.slide_state.ignore_element_line_break = true,
            MarkdownElement::SetexHeading { text } => self.push_slide_title(text),
            MarkdownElement::Heading { level, text } => self.push_heading(level, text),
            MarkdownElement::Paragraph(elements) => self.push_paragraph(elements)?,
            MarkdownElement::List(elements) => self.push_list(elements),
            MarkdownElement::Code(code) => self.push_code(code),
            MarkdownElement::Table(table) => self.push_table(table),
            MarkdownElement::ThematicBreak => self.push_separator(),
            MarkdownElement::Comment { comment, source_position } => self.process_comment(comment, source_position)?,
            MarkdownElement::BlockQuote(lines) => self.push_block_quote(lines),
            MarkdownElement::Image { path, .. } => self.push_image(path)?,
        };
        if should_clear_last {
            self.slide_state.last_element = Default::default();
        }
        Ok(())
    }

    fn process_front_matter(&mut self, contents: &str) -> Result<(), BuildError> {
        let metadata: PresentationMetadata =
            serde_yaml::from_str(contents).map_err(|e| BuildError::InvalidMetadata(e.to_string()))?;

        self.footer_context.borrow_mut().author = metadata.author.clone().unwrap_or_default();
        self.set_theme(&metadata.theme)?;
        if metadata.title.is_some() || metadata.sub_title.is_some() || metadata.author.is_some() {
            self.push_slide_prelude();
            self.push_intro_slide(metadata);
        }
        Ok(())
    }

    fn set_theme(&mut self, metadata: &PresentationThemeMetadata) -> Result<(), BuildError> {
        if metadata.name.is_some() && metadata.path.is_some() {
            return Err(BuildError::InvalidMetadata("cannot have both theme path and theme name".into()));
        }
        if let Some(theme_name) = &metadata.name {
            let theme = PresentationTheme::from_name(theme_name)
                .ok_or_else(|| BuildError::InvalidMetadata(format!("theme '{theme_name}' does not exist")))?;
            self.theme = Cow::Owned(theme);
        }
        if let Some(theme_path) = &metadata.path {
            let theme = self.resources.theme(theme_path)?;
            self.theme = Cow::Owned(theme);
        }
        if let Some(overrides) = &metadata.overrides {
            // This shouldn't fail as the models are already correct.
            let theme = merge_struct::merge(self.theme.as_ref(), overrides)
                .map_err(|e| BuildError::InvalidMetadata(format!("invalid theme: {e}")))?;
            self.theme = Cow::Owned(theme);
        }
        Ok(())
    }

    fn set_code_theme(&mut self) -> Result<(), BuildError> {
        if let Some(theme) = &self.theme.code.theme_name {
            let highlighter = CodeHighlighter::new(theme).map_err(|_| BuildError::InvalidCodeTheme)?;
            self.highlighter = highlighter;
        }
        Ok(())
    }

    fn push_intro_slide(&mut self, metadata: PresentationMetadata) {
        let styles = &self.theme.intro_slide;
        let title = StyledText::new(
            metadata.title.unwrap_or_default().clone(),
            TextStyle::default().bold().colors(styles.title.colors.clone()),
        );
        let sub_title = metadata
            .sub_title
            .as_ref()
            .map(|text| StyledText::new(text.clone(), TextStyle::default().colors(styles.subtitle.colors.clone())));
        let author = metadata
            .author
            .as_ref()
            .map(|text| StyledText::new(text.clone(), TextStyle::default().colors(styles.author.colors.clone())));
        self.chunk_operations.push(RenderOperation::JumpToVerticalCenter);
        self.push_text(Text::from(title), ElementType::PresentationTitle);
        self.push_line_break();
        if let Some(text) = sub_title {
            self.push_text(Text::from(text), ElementType::PresentationSubTitle);
            self.push_line_break();
        }
        if let Some(text) = author {
            match self.theme.intro_slide.author.positioning {
                AuthorPositioning::BelowTitle => {
                    self.push_line_break();
                    self.push_line_break();
                    self.push_line_break();
                }
                AuthorPositioning::PageBottom => {
                    self.chunk_operations.push(RenderOperation::JumpToBottomRow { index: 0 });
                }
            };
            self.push_text(Text::from(text), ElementType::PresentationAuthor);
        }
        self.terminate_slide();
    }

    fn process_comment(&mut self, comment: String, source_position: SourcePosition) -> Result<(), BuildError> {
        if Self::should_ignore_comment(&comment) {
            return Ok(());
        }
        let comment = match comment.parse::<CommentCommand>() {
            Ok(comment) => comment,
            Err(error) => return Err(BuildError::CommandParse { line: source_position.start.line + 1, error }),
        };
        match comment {
            CommentCommand::Pause => self.process_pause(),
            CommentCommand::EndSlide => self.terminate_slide(),
            CommentCommand::InitColumnLayout(columns) => {
                Self::validate_column_layout(&columns)?;
                self.slide_state.layout = LayoutState::InLayout { columns_count: columns.len() };
                self.chunk_operations.push(RenderOperation::InitColumnLayout { columns });
                self.slide_state.needs_enter_column = true;
            }
            CommentCommand::ResetLayout => {
                self.slide_state.layout = LayoutState::Default;
                self.chunk_operations.extend([RenderOperation::ExitLayout, RenderOperation::RenderLineBreak]);
            }
            CommentCommand::Column(column) => {
                let (current_column, columns_count) = match self.slide_state.layout {
                    LayoutState::InColumn { column, columns_count } => (Some(column), columns_count),
                    LayoutState::InLayout { columns_count } => (None, columns_count),
                    LayoutState::Default => return Err(BuildError::NoLayout),
                };
                if current_column == Some(column) {
                    return Err(BuildError::AlreadyInColumn);
                } else if column >= columns_count {
                    return Err(BuildError::ColumnIndexTooLarge);
                }
                self.slide_state.layout = LayoutState::InColumn { column, columns_count };
                self.chunk_operations.push(RenderOperation::EnterColumn { column });
            }
        };
        // Don't push line breaks for any comments.
        self.slide_state.ignore_element_line_break = true;
        Ok(())
    }

    fn should_ignore_comment(comment: &str) -> bool {
        // Ignore any multi line comment; those are assumed to be user comments
        if comment.contains('\n') {
            return true;
        }
        // Ignore vim-like code folding tags
        let comment = comment.trim();
        comment == "{{{" || comment == "}}}"
    }

    fn validate_column_layout(columns: &[u8]) -> Result<(), BuildError> {
        if columns.is_empty() {
            Err(BuildError::InvalidLayout("need at least one column"))
        } else if columns.iter().any(|column| column == &0) {
            Err(BuildError::InvalidLayout("can't have zero sized columns"))
        } else {
            Ok(())
        }
    }

    fn process_pause(&mut self) {
        self.slide_state.last_chunk_ended_in_list = matches!(self.slide_state.last_element, LastElement::List { .. });

        let chunk_operations = mem::take(&mut self.chunk_operations);
        let mutators = mem::take(&mut self.chunk_mutators);
        self.slide_chunks.push(SlideChunk::new(chunk_operations, mutators));
    }

    fn push_slide_title(&mut self, mut text: Text) {
        let style = self.theme.slide_title.clone();
        text.apply_style(&TextStyle::default().bold().colors(style.colors.clone()));

        for _ in 0..style.padding_top.unwrap_or(0) {
            self.push_line_break();
        }
        self.push_text(text, ElementType::SlideTitle);
        self.push_line_break();

        for _ in 0..style.padding_bottom.unwrap_or(0) {
            self.push_line_break();
        }
        if style.separator {
            self.chunk_operations.push(RenderSeparator::default().into());
        }
        self.push_line_break();
        self.slide_state.ignore_element_line_break = true;
    }

    fn push_heading(&mut self, level: u8, mut text: Text) {
        let (element_type, style) = match level {
            1 => (ElementType::Heading1, &self.theme.headings.h1),
            2 => (ElementType::Heading2, &self.theme.headings.h2),
            3 => (ElementType::Heading3, &self.theme.headings.h3),
            4 => (ElementType::Heading4, &self.theme.headings.h4),
            5 => (ElementType::Heading5, &self.theme.headings.h5),
            6 => (ElementType::Heading6, &self.theme.headings.h6),
            other => panic!("unexpected heading level {other}"),
        };
        if let Some(prefix) = &style.prefix {
            let mut prefix = prefix.clone();
            prefix.push(' ');
            text.chunks.insert(0, StyledText::from(prefix));
        }
        let text_style = TextStyle::default().bold().colors(style.colors.clone());
        text.apply_style(&text_style);

        self.push_text(text, element_type);
        self.push_line_break();
    }

    fn push_paragraph(&mut self, elements: Vec<ParagraphElement>) -> Result<(), BuildError> {
        for element in elements {
            match element {
                ParagraphElement::Text(text) => {
                    self.push_text(text, ElementType::Paragraph);
                    self.push_line_break();
                }
                ParagraphElement::LineBreak => {
                    // Line breaks are already pushed after every text chunk.
                }
            };
        }
        Ok(())
    }

    fn push_separator(&mut self) {
        self.chunk_operations.extend([RenderSeparator::default().into(), RenderOperation::RenderLineBreak]);
    }

    fn push_image(&mut self, path: PathBuf) -> Result<(), BuildError> {
        let image = self.resources.image(&path)?;
        self.chunk_operations.push(RenderOperation::RenderImage(image));
        self.chunk_operations.push(RenderOperation::SetColors(self.theme.default_style.colors.clone()));
        Ok(())
    }

    fn push_list(&mut self, list: Vec<ListItem>) {
        let last_chunk_operation = self.slide_chunks.last().and_then(|chunk| chunk.iter_operations().last());
        // If the last chunk ended in a list, pop the newline so we get them all next to each
        // other.
        if matches!(last_chunk_operation, Some(RenderOperation::RenderLineBreak))
            && self.slide_state.last_chunk_ended_in_list
            && self.chunk_operations.is_empty()
        {
            self.slide_chunks.last_mut().unwrap().pop_last();
        }
        // If this chunk just starts (because there was a pause), pick up from the last index.
        let start_index = match self.slide_state.last_element {
            LastElement::List { last_index } if self.chunk_operations.is_empty() => last_index + 1,
            _ => 0,
        };

        let iter = ListIterator::new(list, start_index);
        for item in iter {
            self.push_list_item(item.index, item.item);
        }
    }

    fn push_list_item(&mut self, index: usize, item: ListItem) {
        let padding_length = (item.depth as usize + 1) * 3;
        let mut prefix: String = " ".repeat(padding_length);
        match item.item_type {
            ListItemType::Unordered => {
                let delimiter = match item.depth {
                    0 => '•',
                    1 => '◦',
                    _ => '▪',
                };
                prefix.push(delimiter);
            }
            ListItemType::OrderedParens => {
                prefix.push_str(&(index + 1).to_string());
                prefix.push_str(") ");
            }
            ListItemType::OrderedPeriod => {
                prefix.push_str(&(index + 1).to_string());
                prefix.push_str(". ");
            }
        };

        let prefix_length = prefix.len() as u16;
        self.push_text(prefix.into(), ElementType::List);

        let text = item.contents;
        self.push_aligned_text(text, Alignment::Left { margin: Margin::Fixed(prefix_length) });
        self.push_line_break();
        if item.depth == 0 {
            self.slide_state.last_element = LastElement::List { last_index: index };
        }
    }

    fn push_block_quote(&mut self, lines: Vec<String>) {
        let prefix = self.theme.block_quote.prefix.clone().unwrap_or_default();
        let block_length = lines.iter().map(|line| line.width() + prefix.width()).max().unwrap_or(0);

        self.chunk_operations.push(RenderOperation::SetColors(self.theme.block_quote.colors.clone()));
        for mut line in lines {
            line.insert_str(0, &prefix);

            let line_length = line.width();
            self.chunk_operations.push(RenderOperation::RenderPreformattedLine(PreformattedLine {
                text: line,
                unformatted_length: line_length,
                block_length,
                alignment: self.theme.alignment(&ElementType::BlockQuote).clone(),
            }));
            self.push_line_break();
        }
        self.chunk_operations.push(RenderOperation::SetColors(self.theme.default_style.colors.clone()));
    }

    fn push_text(&mut self, text: Text, element_type: ElementType) {
        let alignment = self.theme.alignment(&element_type);
        self.push_aligned_text(text, alignment);
    }

    fn push_aligned_text(&mut self, text: Text, alignment: Alignment) {
        let mut texts: Vec<WeightedText> = Vec::new();
        for mut chunk in text.chunks {
            if chunk.style.is_code() {
                chunk.style.colors = self.theme.inline_code.colors.clone();
            }
            texts.push(chunk.into());
        }
        if !texts.is_empty() {
            self.chunk_operations
                .push(RenderOperation::RenderText { line: WeightedLine::from(texts), alignment: alignment.clone() });
        }
    }

    fn push_line_break(&mut self) {
        self.chunk_operations.push(RenderOperation::RenderLineBreak);
    }

    fn push_code(&mut self, code: Code) {
        let (lines, context) = self.highlight_lines(&code);
        for line in lines {
            self.chunk_operations.push(RenderOperation::RenderDynamic(Rc::new(line)));
        }
        if self.options.allow_mutations && context.borrow().groups.len() > 1 {
            self.chunk_mutators.push(Box::new(HighlightMutator { context }));
        }
        if code.attributes.execute {
            self.push_code_execution(code);
        }
    }

    fn highlight_lines(&self, code: &Code) -> (Vec<HighlightedLine>, Rc<RefCell<HighlightContext>>) {
        let lines = CodePreparer { theme: &self.theme }.prepare(code);
        let block_length = lines.iter().map(|line| line.width()).max().unwrap_or(0);
        let mut empty_highlighter = self.highlighter.language_highlighter(&CodeLanguage::Unknown);
        let mut code_highlighter = self.highlighter.language_highlighter(&code.language);
        let padding_style = {
            let mut highlighter = self.highlighter.language_highlighter(&CodeLanguage::Rust);
            highlighter.style_line("//").first().expect("no styles").style
        };
        let groups = match self.options.allow_mutations {
            true => code.attributes.highlight_groups.clone(),
            false => vec![HighlightGroup::new(vec![Highlight::All])],
        };
        let context = Rc::new(RefCell::new(HighlightContext {
            groups,
            current: 0,
            block_length,
            alignment: self.theme.alignment(&ElementType::Code),
        }));

        let mut output = Vec::new();
        for line in lines.into_iter() {
            let highlighted = line.highlight(&padding_style, &mut code_highlighter);
            let not_highlighted = line.highlight(&padding_style, &mut empty_highlighter);
            let width = line.width();
            let line_number = line.line_number;
            let context = context.clone();
            output.push(HighlightedLine { highlighted, not_highlighted, line_number, width, context });
        }
        (output, context)
    }

    fn push_code_execution(&mut self, code: Code) {
        let operation = RunCodeOperation::new(
            code,
            self.theme.default_style.colors.clone(),
            self.theme.execution_output.colors.clone(),
        );
        let operation = RenderOperation::RenderOnDemand(Rc::new(operation));
        self.chunk_operations.push(operation);
    }

    fn terminate_slide(&mut self) {
        let footer = self.generate_footer();

        let operations = mem::take(&mut self.chunk_operations);
        let mutators = mem::take(&mut self.chunk_mutators);
        self.slide_chunks.push(SlideChunk::new(operations, mutators));

        let chunks = mem::take(&mut self.slide_chunks);
        self.slides.push(Slide::new(chunks, footer));
        self.push_slide_prelude();
        self.slide_state = Default::default();
    }

    fn generate_footer(&mut self) -> Vec<RenderOperation> {
        let generator = FooterGenerator {
            style: self.theme.footer.clone(),
            current_slide: self.slides.len(),
            context: self.footer_context.clone(),
        };
        vec![
            // Exit any layout we're in so this gets rendered on a default screen size.
            RenderOperation::ExitLayout,
            // Pop the slide margin so we're at the terminal rect.
            RenderOperation::PopMargin,
            RenderOperation::RenderDynamic(Rc::new(generator)),
        ]
    }

    fn push_table(&mut self, table: Table) {
        let widths: Vec<_> = (0..table.columns())
            .map(|column| table.iter_column(column).map(|text| text.width()).max().unwrap_or(0))
            .collect();
        let flattened_header = Self::prepare_table_row(table.header, &widths);
        self.push_text(flattened_header, ElementType::Table);
        self.push_line_break();

        let mut separator = Text { chunks: Vec::new() };
        for (index, width) in widths.iter().enumerate() {
            let mut contents = String::new();
            let mut margin = 1;
            if index > 0 {
                contents.push('┼');
                // Append an extra dash to have 1 column margin on both sides
                if index < widths.len() - 1 {
                    margin += 1;
                }
            }
            contents.extend(iter::repeat("─").take(*width + margin));
            separator.chunks.push(StyledText::from(contents));
        }

        self.push_text(separator, ElementType::Table);
        self.push_line_break();

        for row in table.rows {
            let flattened_row = Self::prepare_table_row(row, &widths);
            self.push_text(flattened_row, ElementType::Table);
            self.push_line_break();
        }
    }

    fn prepare_table_row(row: TableRow, widths: &[usize]) -> Text {
        let mut flattened_row = Text { chunks: Vec::new() };
        for (column, text) in row.0.into_iter().enumerate() {
            if column > 0 {
                flattened_row.chunks.push(StyledText::from(" │ "));
            }
            let text_length = text.width();
            flattened_row.chunks.extend(text.chunks.into_iter());

            let cell_width = widths[column];
            if text_length < cell_width {
                let padding = " ".repeat(cell_width - text_length);
                flattened_row.chunks.push(StyledText::from(padding));
            }
        }
        flattened_row
    }
}

struct CodePreparer<'a> {
    theme: &'a PresentationTheme,
}

impl<'a> CodePreparer<'a> {
    fn prepare(&self, code: &Code) -> Vec<CodeLine> {
        let mut lines = Vec::new();
        let horizontal_padding = self.theme.code.padding.horizontal.unwrap_or(0);
        let vertical_padding = self.theme.code.padding.vertical.unwrap_or(0);
        if vertical_padding > 0 {
            lines.push(CodeLine::empty());
        }
        self.push_lines(code, horizontal_padding, &mut lines);
        if vertical_padding > 0 {
            lines.push(CodeLine::empty());
        }
        lines
    }

    fn push_lines(&self, code: &Code, horizontal_padding: u8, lines: &mut Vec<CodeLine>) {
        if code.contents.is_empty() {
            return;
        }

        let padding = " ".repeat(horizontal_padding as usize);
        let total_lines_width = code.contents.lines().count().ilog10();
        for (index, line) in code.contents.lines().enumerate() {
            let mut line = line.to_string();
            let mut prefix = padding.clone();
            if code.attributes.line_numbers {
                let line_number = index + 1;
                let line_number_width = line_number.ilog10();
                // Suffix this with padding to make all numbers pad to the right
                let number_padding = total_lines_width - line_number_width;
                prefix.push_str(&" ".repeat(number_padding as usize));
                prefix.push_str(&line_number.to_string());
                prefix.push(' ');
            }
            line.push('\n');
            let line_number = Some(index as u16 + 1);
            lines.push(CodeLine { prefix, code: line, suffix: padding.clone(), line_number });
        }
    }
}

struct CodeLine {
    prefix: String,
    code: String,
    suffix: String,
    line_number: Option<u16>,
}

impl CodeLine {
    fn empty() -> Self {
        Self { prefix: String::new(), code: "\n".into(), suffix: String::new(), line_number: None }
    }

    fn width(&self) -> usize {
        self.prefix.width() + self.code.width() + self.suffix.width()
    }

    fn highlight(&self, padding_style: &Style, code_highlighter: &mut LanguageHighlighter) -> String {
        let mut output = StyledTokens { style: *padding_style, tokens: &self.prefix }.apply_style();
        output.push_str(&code_highlighter.highlight_line(&self.code));
        // Remove newline
        output.pop();
        output.push_str(&StyledTokens { style: *padding_style, tokens: &self.suffix }.apply_style());
        output
    }
}

#[derive(Debug)]
struct HighlightContext {
    groups: Vec<HighlightGroup>,
    current: usize,
    block_length: usize,
    alignment: Alignment,
}

#[derive(Debug)]
struct HighlightedLine {
    highlighted: String,
    not_highlighted: String,
    line_number: Option<u16>,
    width: usize,
    context: Rc<RefCell<HighlightContext>>,
}

impl AsRenderOperations for HighlightedLine {
    fn as_render_operations(&self, _: &WindowSize) -> Vec<RenderOperation> {
        let context = self.context.borrow();
        let group = &context.groups[context.current];
        let needs_highlight = self.line_number.map(|number| group.contains(number)).unwrap_or_default();
        // TODO: Cow<str>?
        let text = match needs_highlight {
            true => self.highlighted.clone(),
            false => self.not_highlighted.clone(),
        };
        vec![
            RenderOperation::RenderPreformattedLine(PreformattedLine {
                text,
                unformatted_length: self.width,
                block_length: context.block_length,
                alignment: context.alignment.clone(),
            }),
            RenderOperation::RenderLineBreak,
        ]
    }

    fn diffable_content(&self) -> Option<&str> {
        Some(&self.highlighted)
    }
}

#[derive(Debug)]
struct HighlightMutator {
    context: Rc<RefCell<HighlightContext>>,
}

impl ChunkMutator for HighlightMutator {
    fn mutate_next(&self) -> bool {
        let mut context = self.context.borrow_mut();
        if context.current == context.groups.len() - 1 {
            false
        } else {
            context.current += 1;
            true
        }
    }

    fn mutate_previous(&self) -> bool {
        let mut context = self.context.borrow_mut();
        if context.current == 0 {
            false
        } else {
            context.current -= 1;
            true
        }
    }

    fn reset_mutations(&self) {
        self.context.borrow_mut().current = 0;
    }

    fn apply_all_mutations(&self) {
        let mut context = self.context.borrow_mut();
        context.current = context.groups.len() - 1;
    }

    fn mutations(&self) -> (usize, usize) {
        let context = self.context.borrow();
        (context.current, context.groups.len())
    }
}

#[derive(Debug, Default)]
struct SlideState {
    ignore_element_line_break: bool,
    needs_enter_column: bool,
    last_chunk_ended_in_list: bool,
    last_element: LastElement,
    layout: LayoutState,
}

#[derive(Debug, Default)]
enum LayoutState {
    #[default]
    Default,
    InLayout {
        columns_count: usize,
    },
    InColumn {
        column: usize,
        columns_count: usize,
    },
}

#[derive(Debug, Default)]
enum LastElement {
    #[default]
    Any,
    List {
        last_index: usize,
    },
}

#[derive(Debug, Default)]
struct FooterContext {
    total_slides: usize,
    author: String,
}

#[derive(Debug)]
struct FooterGenerator {
    current_slide: usize,
    context: Rc<RefCell<FooterContext>>,
    style: FooterStyle,
}

impl FooterGenerator {
    fn render_template(
        template: &str,
        current_slide: &str,
        context: &FooterContext,
        colors: Colors,
        alignment: Alignment,
    ) -> RenderOperation {
        let contents = template
            .replace("{current_slide}", current_slide)
            .replace("{total_slides}", &context.total_slides.to_string())
            .replace("{author}", &context.author);
        let text = WeightedText::from(StyledText::new(contents, TextStyle::default().colors(colors)));
        RenderOperation::RenderText { line: vec![text].into(), alignment }
    }
}

impl AsRenderOperations for FooterGenerator {
    fn as_render_operations(&self, dimensions: &WindowSize) -> Vec<RenderOperation> {
        let context = self.context.borrow();
        match &self.style {
            FooterStyle::Template { left, center, right, colors } => {
                let current_slide = (self.current_slide + 1).to_string();
                // We print this one row below the bottom so there's one row of padding.
                let mut operations = vec![RenderOperation::JumpToBottomRow { index: 1 }];
                let margin = Margin::Fixed(1);
                let alignments = [
                    Alignment::Left { margin: margin.clone() },
                    Alignment::Center { minimum_size: 0, minimum_margin: margin.clone() },
                    Alignment::Right { margin: margin.clone() },
                ];
                for (text, alignment) in [left, center, right].iter().zip(alignments) {
                    if let Some(text) = text {
                        operations.push(Self::render_template(
                            text,
                            &current_slide,
                            &context,
                            colors.clone(),
                            alignment,
                        ));
                    }
                }
                operations
            }
            FooterStyle::ProgressBar { character, colors } => {
                let character = character.unwrap_or('█').to_string();
                let total_columns = dimensions.columns as usize / character.width();
                let progress_ratio = (self.current_slide + 1) as f64 / context.total_slides as f64;
                let columns_ratio = (total_columns as f64 * progress_ratio).ceil();
                let bar = character.repeat(columns_ratio as usize);
                let bar = vec![WeightedText::from(StyledText::new(bar, TextStyle::default().colors(colors.clone())))];
                vec![
                    RenderOperation::JumpToBottomRow { index: 0 },
                    RenderOperation::RenderText {
                        line: bar.into(),
                        alignment: Alignment::Left { margin: Margin::Fixed(0) },
                    },
                ]
            }
            FooterStyle::Empty => vec![],
        }
    }

    fn diffable_content(&self) -> Option<&str> {
        None
    }
}

/// An error when building a presentation.
#[derive(thiserror::Error, Debug)]
pub enum BuildError {
    #[error("loading image: {0}")]
    LoadImage(#[from] LoadImageError),

    #[error("invalid presentation metadata: {0}")]
    InvalidMetadata(String),

    #[error("invalid theme: {0}")]
    InvalidTheme(#[from] LoadThemeError),

    #[error("invalid code highlighter theme")]
    InvalidCodeTheme,

    #[error("invalid layout: {0}")]
    InvalidLayout(&'static str),

    #[error("can't enter layout: no layout defined")]
    NoLayout,

    #[error("can't enter layout column: already in it")]
    AlreadyInColumn,

    #[error("can't enter layout column: column index too large")]
    ColumnIndexTooLarge,

    #[error("need to enter layout column explicitly using `column` command")]
    NotInsideColumn,

    #[error("error parsing command at line {line}: {error}")]
    CommandParse { line: usize, error: CommandParseError },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CommentCommand {
    Pause,
    EndSlide,
    #[serde(rename = "column_layout")]
    InitColumnLayout(Vec<u8>),
    Column(usize),
    ResetLayout,
}

impl FromStr for CommentCommand {
    type Err = CommandParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        #[derive(Deserialize)]
        struct CommandWrapper(#[serde(with = "serde_yaml::with::singleton_map")] CommentCommand);

        let wrapper = serde_yaml::from_str::<CommandWrapper>(s)?;
        Ok(wrapper.0)
    }
}

#[derive(thiserror::Error, Debug)]
pub struct CommandParseError(#[from] serde_yaml::Error);

impl Display for CommandParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.0.to_string();
        // Remove the trailing "at line X, ..." that comes from serde_yaml. This otherwise claims
        // we're always in line 1 because the yaml is parsed in isolation out of the HTML comment.
        let inner = inner.split(" at line").next().unwrap();
        write!(f, "{inner}")
    }
}

#[derive(Debug)]
struct RunCodeOperationInner {
    handle: Option<ExecutionHandle>,
    output_lines: Vec<String>,
    state: RenderOnDemandState,
}

#[derive(Debug)]
pub(crate) struct RunCodeOperation {
    code: Code,
    default_colors: Colors,
    block_colors: Colors,
    inner: Rc<RefCell<RunCodeOperationInner>>,
}

impl RunCodeOperation {
    fn new(code: Code, default_colors: Colors, block_colors: Colors) -> Self {
        let inner =
            RunCodeOperationInner { handle: None, output_lines: Vec::new(), state: RenderOnDemandState::default() };
        Self { code, default_colors, block_colors, inner: Rc::new(RefCell::new(inner)) }
    }

    fn render_line(&self, line: String) -> RenderOperation {
        let line_len = line.len();
        RenderOperation::RenderPreformattedLine(PreformattedLine {
            text: line,
            unformatted_length: line_len,
            block_length: line_len,
            alignment: Default::default(),
        })
    }
}

impl AsRenderOperations for RunCodeOperation {
    fn as_render_operations(&self, dimensions: &WindowSize) -> Vec<RenderOperation> {
        let inner = self.inner.borrow();
        if matches!(inner.state, RenderOnDemandState::NotStarted) {
            return Vec::new();
        }
        let state = match inner.state {
            RenderOnDemandState::Rendered => "done",
            _ => "running",
        };
        let heading = format!(" [{state}] ");
        let separator = RenderSeparator::new(heading);
        let mut operations = vec![
            RenderOperation::RenderLineBreak,
            RenderOperation::RenderDynamic(Rc::new(separator)),
            RenderOperation::RenderLineBreak,
            RenderOperation::RenderLineBreak,
            RenderOperation::SetColors(self.block_colors.clone()),
        ];

        for line in &inner.output_lines {
            let chunks = line.chars().chunks(dimensions.columns as usize);
            for chunk in &chunks {
                operations.push(self.render_line(chunk.collect()));
                operations.push(RenderOperation::RenderLineBreak);
            }
        }
        operations.push(RenderOperation::SetColors(self.default_colors.clone()));
        operations
    }

    fn diffable_content(&self) -> Option<&str> {
        None
    }
}

impl RenderOnDemand for RunCodeOperation {
    fn poll_state(&self) -> RenderOnDemandState {
        let mut inner = self.inner.borrow_mut();
        if let Some(handle) = inner.handle.as_mut() {
            let state = handle.state();
            let ExecutionState { output, status } = state;
            if status.is_finished() {
                inner.handle.take();
                inner.state = RenderOnDemandState::Rendered;
            }
            inner.output_lines = output;
            if matches!(status, ProcessStatus::Failure) {
                inner.output_lines.push("[finished with error]".to_string());
            }
        }
        inner.state.clone()
    }

    fn start_render(&self) -> bool {
        let mut inner = self.inner.borrow_mut();
        if !matches!(inner.state, RenderOnDemandState::NotStarted) {
            return false;
        }
        match CodeExecuter::execute(&self.code) {
            Ok(handle) => {
                inner.handle = Some(handle);
                inner.state = RenderOnDemandState::Rendering;
                true
            }
            Err(e) => {
                inner.output_lines = vec![e.to_string()];
                inner.state = RenderOnDemandState::Rendered;
                true
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
struct RenderSeparator {
    heading: String,
}

impl RenderSeparator {
    fn new<S: Into<String>>(heading: S) -> Self {
        Self { heading: heading.into() }
    }
}

impl From<RenderSeparator> for RenderOperation {
    fn from(separator: RenderSeparator) -> Self {
        Self::RenderDynamic(Rc::new(separator))
    }
}

impl AsRenderOperations for RenderSeparator {
    fn as_render_operations(&self, dimensions: &WindowSize) -> Vec<RenderOperation> {
        let character = "—";
        let separator = match self.heading.is_empty() {
            true => character.repeat(dimensions.columns as usize),
            false => {
                let dashes_len = (dimensions.columns as usize).saturating_sub(self.heading.len()) / 2;
                let dashes = character.repeat(dashes_len);
                let heading = &self.heading;
                format!("{dashes}{heading}{dashes}")
            }
        };
        vec![RenderOperation::RenderText { line: separator.into(), alignment: Default::default() }]
    }

    fn diffable_content(&self) -> Option<&str> {
        None
    }
}

struct ListIterator<I> {
    remaining: I,
    next_index: usize,
    current_depth: u8,
    saved_indexes: Vec<usize>,
}

impl<I> ListIterator<I> {
    fn new<T>(remaining: T, next_index: usize) -> Self
    where
        I: Iterator<Item = ListItem>,
        T: IntoIterator<IntoIter = I, Item = ListItem>,
    {
        Self { remaining: remaining.into_iter(), next_index, current_depth: 0, saved_indexes: Vec::new() }
    }
}

impl<I> Iterator for ListIterator<I>
where
    I: Iterator<Item = ListItem>,
{
    type Item = IndexedListItem;

    fn next(&mut self) -> Option<Self::Item> {
        let head = self.remaining.next()?;
        if head.depth != self.current_depth {
            if head.depth > self.current_depth {
                // If we're going deeper, save the next index so we can continue later on and start
                // from 0.
                self.saved_indexes.push(self.next_index);
                self.next_index = 0;
            } else {
                // if we're getting out, recover the index we had previously saved.
                for _ in head.depth..self.current_depth {
                    self.next_index = self.saved_indexes.pop().unwrap_or(0);
                }
            }
            self.current_depth = head.depth;
        }
        let index = self.next_index;
        self.next_index += 1;
        Some(IndexedListItem { index, item: head })
    }
}

struct IndexedListItem {
    index: usize,
    item: ListItem,
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::markdown::elements::{CodeAttributes, CodeLanguage};
    use rstest::rstest;

    fn build_presentation(elements: Vec<MarkdownElement>) -> Presentation {
        try_build_presentation(elements).expect("build failed")
    }

    fn try_build_presentation(elements: Vec<MarkdownElement>) -> Result<Presentation, BuildError> {
        let highlighter = CodeHighlighter::new("base16-ocean.dark").unwrap();
        let theme = PresentationTheme::default();
        let mut resources = Resources::new("/tmp");
        let options = PresentationBuilderOptions::default();
        let builder = PresentationBuilder::new(highlighter, &theme, &mut resources, options);
        builder.build(elements)
    }

    fn build_pause() -> MarkdownElement {
        MarkdownElement::Comment { comment: "pause".into(), source_position: Default::default() }
    }

    fn build_end_slide() -> MarkdownElement {
        MarkdownElement::Comment { comment: "end_slide".into(), source_position: Default::default() }
    }

    fn build_column_layout(width: u8) -> MarkdownElement {
        MarkdownElement::Comment { comment: format!("column_layout: [{width}]"), source_position: Default::default() }
    }

    fn build_column(column: u8) -> MarkdownElement {
        MarkdownElement::Comment { comment: format!("column: {column}"), source_position: Default::default() }
    }

    fn is_visible(operation: &RenderOperation) -> bool {
        use RenderOperation::*;
        match operation {
            ClearScreen
            | SetColors(_)
            | JumpToVerticalCenter
            | JumpToBottomRow { .. }
            | InitColumnLayout { .. }
            | EnterColumn { .. }
            | ExitLayout { .. }
            | ApplyMargin(_)
            | PopMargin => false,
            RenderText { .. }
            | RenderLineBreak
            | RenderImage(_)
            | RenderPreformattedLine(_)
            | RenderDynamic(_)
            | RenderOnDemand(_) => true,
        }
    }

    fn extract_text_lines(operations: &[RenderOperation]) -> Vec<String> {
        let mut output = Vec::new();
        let mut current_line = String::new();
        for operation in operations {
            match operation {
                RenderOperation::RenderText { line, .. } => {
                    let texts: Vec<_> = line.iter_texts().map(|text| text.text.text.clone()).collect();
                    current_line.push_str(&texts.join(""));
                }
                RenderOperation::RenderLineBreak if !current_line.is_empty() => {
                    output.push(mem::take(&mut current_line));
                }
                _ => (),
            };
        }
        if !current_line.is_empty() {
            output.push(current_line);
        }
        output
    }

    fn extract_slide_text_lines(slide: Slide) -> Vec<String> {
        let operations: Vec<_> = slide.into_operations().into_iter().filter(|op| is_visible(op)).collect();
        extract_text_lines(&operations)
    }

    #[test]
    fn prelude_appears_once() {
        let elements = vec![
            MarkdownElement::FrontMatter("author: bob".to_string()),
            MarkdownElement::Heading { text: Text::from("hello"), level: 1 },
            build_end_slide(),
            MarkdownElement::Heading { text: Text::from("bye"), level: 1 },
        ];
        let presentation = build_presentation(elements);
        for (index, slide) in presentation.iter_slides().into_iter().enumerate() {
            let clear_screen_count =
                slide.iter_operations().filter(|op| matches!(op, RenderOperation::ClearScreen)).count();
            let set_colors_count =
                slide.iter_operations().filter(|op| matches!(op, RenderOperation::SetColors(_))).count();
            assert_eq!(clear_screen_count, 1, "{clear_screen_count} clear screens in slide {index}");
            assert_eq!(set_colors_count, 1, "{set_colors_count} clear screens in slide {index}");
        }
    }

    #[test]
    fn slides_start_with_one_newline() {
        let elements = vec![
            MarkdownElement::FrontMatter("author: bob".to_string()),
            MarkdownElement::Heading { text: Text::from("hello"), level: 1 },
            build_end_slide(),
            MarkdownElement::Heading { text: Text::from("bye"), level: 1 },
        ];
        let presentation = build_presentation(elements);
        assert_eq!(presentation.iter_slides().count(), 3);

        // Don't process the intro slide as it's special
        let slides = presentation.into_slides().into_iter().skip(1);
        for slide in slides {
            let mut ops = slide.into_operations().into_iter().filter(is_visible);
            // We should start with a newline
            assert!(matches!(ops.next(), Some(RenderOperation::RenderLineBreak)));
            // And the second one should _not_ be a newline
            assert!(!matches!(ops.next(), Some(RenderOperation::RenderLineBreak)));
        }
    }

    #[test]
    fn table() {
        let elements = vec![MarkdownElement::Table(Table {
            header: TableRow(vec![Text::from("key"), Text::from("value"), Text::from("other")]),
            rows: vec![TableRow(vec![Text::from("potato"), Text::from("bar"), Text::from("yes")])],
        })];
        let slides = build_presentation(elements).into_slides();
        let lines = extract_slide_text_lines(slides.into_iter().next().unwrap());
        let expected_lines = &["key    │ value │ other", "───────┼───────┼──────", "potato │ bar   │ yes  "];
        assert_eq!(lines, expected_lines);
    }

    #[test]
    fn layout_without_init() {
        let elements = vec![build_column(0)];
        let result = try_build_presentation(elements);
        assert!(result.is_err());
    }

    #[test]
    fn already_in_column() {
        let elements = vec![
            MarkdownElement::Comment { comment: "column_layout: [1]".into(), source_position: Default::default() },
            MarkdownElement::Comment { comment: "column: 0".into(), source_position: Default::default() },
            MarkdownElement::Comment { comment: "column: 0".into(), source_position: Default::default() },
        ];
        let result = try_build_presentation(elements);
        assert!(result.is_err());
    }

    #[test]
    fn column_index_overflow() {
        let elements = vec![
            MarkdownElement::Comment { comment: "column_layout: [1]".into(), source_position: Default::default() },
            MarkdownElement::Comment { comment: "column: 1".into(), source_position: Default::default() },
        ];
        let result = try_build_presentation(elements);
        assert!(result.is_err());
    }

    #[rstest]
    #[case::empty("column_layout: []")]
    #[case::zero("column_layout: [0]")]
    #[case::one_is_zero("column_layout: [1, 0]")]
    fn invalid_layouts(#[case] definition: &str) {
        let elements =
            vec![MarkdownElement::Comment { comment: definition.into(), source_position: Default::default() }];
        let result = try_build_presentation(elements);
        assert!(result.is_err());
    }

    #[test]
    fn operation_without_enter_column() {
        let elements = vec![
            MarkdownElement::Comment { comment: "column_layout: [1]".into(), source_position: Default::default() },
            MarkdownElement::ThematicBreak,
        ];
        let result = try_build_presentation(elements);
        assert!(result.is_err());
    }

    #[rstest]
    #[case::pause("pause", CommentCommand::Pause)]
    #[case::pause(" pause ", CommentCommand::Pause)]
    #[case::end_slide("end_slide", CommentCommand::EndSlide)]
    #[case::column_layout("column_layout: [1, 2]", CommentCommand::InitColumnLayout(vec![1, 2]))]
    #[case::column("column: 1", CommentCommand::Column(1))]
    #[case::reset_layout("reset_layout", CommentCommand::ResetLayout)]
    fn command_formatting(#[case] input: &str, #[case] expected: CommentCommand) {
        let parsed: CommentCommand = input.parse().expect("deserialization failed");
        assert_eq!(parsed, expected);
    }

    #[test]
    fn end_slide_inside_layout() {
        let elements = vec![build_column_layout(1), build_end_slide()];
        let presentation = build_presentation(elements);
        assert_eq!(presentation.iter_slides().count(), 2);
    }

    #[test]
    fn end_slide_inside_column() {
        let elements = vec![build_column_layout(1), build_column(0), build_end_slide()];
        let presentation = build_presentation(elements);
        assert_eq!(presentation.iter_slides().count(), 2);
    }

    #[test]
    fn pause_inside_layout() {
        let elements = vec![build_column_layout(1), build_pause(), build_column(0)];
        let presentation = build_presentation(elements);
        assert_eq!(presentation.iter_slides().count(), 1);
    }

    #[test]
    fn iterate_list() {
        let iter = ListIterator::new(
            vec![
                ListItem { depth: 0, contents: "0".into(), item_type: ListItemType::Unordered },
                ListItem { depth: 0, contents: "1".into(), item_type: ListItemType::Unordered },
                ListItem { depth: 1, contents: "00".into(), item_type: ListItemType::Unordered },
                ListItem { depth: 1, contents: "01".into(), item_type: ListItemType::Unordered },
                ListItem { depth: 1, contents: "02".into(), item_type: ListItemType::Unordered },
                ListItem { depth: 2, contents: "001".into(), item_type: ListItemType::Unordered },
                ListItem { depth: 0, contents: "2".into(), item_type: ListItemType::Unordered },
            ],
            0,
        );
        let expected_indexes = [0, 1, 0, 1, 2, 0, 2];
        let indexes: Vec<_> = iter.map(|item| item.index).collect();
        assert_eq!(indexes, expected_indexes);
    }

    #[test]
    fn iterate_list_starting_from_other() {
        let list = ListIterator::new(
            vec![
                ListItem { depth: 0, contents: "0".into(), item_type: ListItemType::Unordered },
                ListItem { depth: 0, contents: "1".into(), item_type: ListItemType::Unordered },
            ],
            3,
        );
        let expected_indexes = [3, 4];
        let indexes: Vec<_> = list.into_iter().map(|item| item.index).collect();
        assert_eq!(indexes, expected_indexes);
    }

    #[test]
    fn ordered_list_with_pauses() {
        let elements = vec![
            MarkdownElement::List(vec![
                ListItem { depth: 0, contents: "one".into(), item_type: ListItemType::OrderedPeriod },
                ListItem { depth: 1, contents: "one_one".into(), item_type: ListItemType::OrderedPeriod },
                ListItem { depth: 1, contents: "one_two".into(), item_type: ListItemType::OrderedPeriod },
            ]),
            build_pause(),
            MarkdownElement::List(vec![ListItem {
                depth: 0,
                contents: "two".into(),
                item_type: ListItemType::OrderedPeriod,
            }]),
        ];
        let slides = build_presentation(elements).into_slides();
        let lines = extract_slide_text_lines(slides.into_iter().next().unwrap());
        let expected_lines = &["   1. one", "      1. one_one", "      2. one_two", "   2. two"];
        assert_eq!(lines, expected_lines);
    }

    #[test]
    fn pause_after_list() {
        let elements = vec![
            MarkdownElement::List(vec![ListItem {
                depth: 0,
                contents: "one".into(),
                item_type: ListItemType::OrderedPeriod,
            }]),
            build_pause(),
            MarkdownElement::Heading { level: 1, text: "hi".into() },
            MarkdownElement::List(vec![ListItem {
                depth: 0,
                contents: "two".into(),
                item_type: ListItemType::OrderedPeriod,
            }]),
        ];
        let slides = build_presentation(elements).into_slides();
        let first_chunk = &slides[0];
        let operations = first_chunk.iter_operations().collect::<Vec<_>>();
        // This is pretty easy to break, refactor soon
        let last_operation = &operations[operations.len() - 4];
        assert!(matches!(last_operation, RenderOperation::RenderLineBreak), "last operation is {last_operation:?}");
    }

    #[rstest]
    #[case::multiline("hello\nworld")]
    #[case::many_open_braces("{{{")]
    #[case::many_close_braces("}}}")]
    fn ignore_comments(#[case] comment: &str) {
        assert!(PresentationBuilder::should_ignore_comment(comment));
    }

    #[test]
    fn code_with_line_numbers() {
        let total_lines = 11;
        let input_lines = "hi\n".repeat(total_lines);
        let code = Code {
            contents: input_lines,
            language: CodeLanguage::Unknown,
            attributes: CodeAttributes { line_numbers: true, ..Default::default() },
        };
        let lines = CodePreparer { theme: &Default::default() }.prepare(&code);
        assert_eq!(lines.len(), total_lines);

        let mut lines = lines.into_iter().enumerate();
        // 0..=9
        for (index, line) in lines.by_ref().take(9) {
            let line_number = index + 1;
            assert_eq!(&line.prefix, &format!(" {line_number} "));
        }
        // 10..
        for (index, line) in lines {
            let line_number = index + 1;
            assert_eq!(&line.prefix, &format!("{line_number} "));
        }
    }
}
