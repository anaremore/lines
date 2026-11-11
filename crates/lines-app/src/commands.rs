//! Workspace commands and the keys bound to them (decision 0018: every action is a typed
//! command; hotkeys, menus, header controls and the command palette are just ways to issue
//! one). One table ([`BINDINGS`]) gives both the keymap and the hints shown in menus and the
//! palette, so they can't disagree. Pure and tested; the workspace executes the commands.
//!
//! "Ctrl" means Cmd on macOS (GPUI's `secondary` modifier).

use gpui::Keystroke;
use lines_core::Timeframe;
use lines_core::drawings::Tool;

use crate::layout::{Axis, Direction, LinkGroup, PaneKind, Preset, link_letter};
use crate::pane_columns::{Column, ColumnCmd, DomCol, JournalCol, TapeCol};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Move keyboard focus to the neighbouring pane.
    Focus(Direction),
    /// Split the focused pane; the new pane (same kind, symbol and link) goes right or below.
    Split(Axis),
    ClosePane,
    /// Change the focused pane's kind, keeping its symbol and link.
    SetKind(PaneKind),
    /// Cycle the focused pane's link group: unlinked → A → B → C → D.
    CycleLink,
    SetLink(LinkGroup),
    /// Replace the active tab's layout with a preset, on the focused pane's symbol.
    Preset(Preset),
    /// Hide the focused DOM or time & sales.
    HidePane,
    /// Hide or show the DOMs (or tapes) that go with the focused pane (`crate::tabstate`).
    Toggle(PaneKind),
    /// The focused chart's "chart only": hide or restore its linked DOM and time & sales.
    ChartOnly,
    /// Show every hidden pane in the active tab.
    ShowHidden,
    /// Put the focused pane in its own window (in that window: re-attach it).
    PopOut,
    /// Return a detached pane to its previous spot (from a main window: all of the tab's).
    Reattach,
    /// Dock a detached pane beside the chart that last had focus.
    DockBesideChart,
    NewTab,
    CloseTab,
    /// Name the active tab (empty: named after its symbols again).
    RenameTab,
    /// Pin or unpin the active tab: Close others and a middle click skip a pinned tab.
    TogglePinTab,
    /// Close every tab of the window but the active one and the pinned ones.
    CloseOtherTabs,
    /// Reopen the most recently closed tab.
    ReopenTab,
    NextTab,
    PrevTab,
    SelectTab(usize),
    /// Move the active tab into a new window.
    TabToNewWindow,
    ToggleCrosshairSync,
    /// Crosshair sync carries the price too, between charts of the same product.
    ToggleCrosshairSyncPrice,
    /// The focused chart's magnet crosshair (snap to the bar's open/high/low/close).
    ToggleMagnet,
    /// The focused chart's order strip (chart trading phase 2b): market and crosshair limits.
    ToggleOrderStrip,
    /// The focused pane's context menu, from the keyboard.
    Menu,
    /// Every command, searchable.
    Palette,
    SaveAs,
    Open,
    /// Restart into the downloaded, verified update (held while anything is trading).
    RestartToUpdate,
    /// Go back to the version the last update replaced, with its saved files.
    RollBackUpdate,
    /// Every pop-up panel opens where it did before it was dragged.
    ResetPanels,
    /// The focused chart: hide (or show) its study legends and tags; studies stay drawn.
    ToggleStudyLabels,
    /// The focused chart: hide (or show) all its studies, keeping their configuration.
    ToggleStudies,
    /// The focused chart without legends or study tags; again restores them.
    CleanChart,
    /// Trade history on the focused chart (`trade_marks`): shown or hidden (Alt+E).
    ToggleTrades,
    /// Trade history shown (`true`) or hidden on every open chart: every window, popped-out
    /// panes and hidden tabs (`workspace::trades_on_all_charts`).
    TradesOnAllCharts(bool),
    /// Its entry–exit lines, on or off.
    ToggleTradeLines,
    /// Its labels, on or off.
    ToggleTradeLabels,
    /// Every account's trades (`true`) or only this pane's (the menu's picks).
    TradeAccounts(bool),
    /// Which trades, by trade date.
    TradePeriod(crate::trade_marks::Period),
    /// Every key and its command: Settings › Keys (F1).
    ShowKeys,
    /// Show or hide one of the footer's links (its right-click menu; safety items can't be).
    FooterLink(crate::footer::LinkKind),
    /// A column action on the focused DOM or time & sales (`crate::pane_columns`).
    Columns(ColumnCmd),
    /// The focused unavailable pane shows the source's own symbol (one click, also Enter).
    ShowSourceHere,
    /// Every unavailable pane in the tab shows the source's own symbol.
    ShowSourceInTab,
    /// Write a diagnostics zip (logs, versions, redacted settings, feed and frame stats).
    ExportDiagnostics,
    /// The "Download days…" window (decision 0030 §8): whole past trade dates from a source
    /// that can download them, into the local store. No key: palette only.
    DownloadDays,
    /// The focused chart: hide (or show) all external levels (decision 0022).
    ToggleLevels,
    /// The focused chart: hide (or show) level labels and level legend rows.
    ToggleLevelLabels,
    /// Show or hide the chart headers' performance stats (feed msgs/s, updates/s, paints/s,
    /// paint time): a workspace preference, off by default. No key: palette and menu only.
    TogglePerfStats,
    /// Show or hide the latency HUD in chart headers (roadmap 6b: feed, paint, order and ack
    /// latency, the clock offset): a workspace preference, off by default; nothing is
    /// recorded while it is hidden. No key: palette and menu only.
    ToggleLatencyHud,
    /// The focused DOM's price trail on/off (its column comes with it). No key: pane keys
    /// are reserved; palette and the DOM's pane menu.
    DomTrail,
    /// The focused DOM's trail half-life: the next preset (0.5, 1, 1.5, 3, 5 s).
    DomTrailHalfLife,
    /// Show the trade journal: focus its pane, else open one in a new tab (roadmap 8c).
    OpenJournal,
    /// Show an option chain: focus one, else open one in a new tab (decision 0028).
    OpenChain,
    /// Show the positions pane: focus one, else open one in a new tab.
    OpenPositions,
    /// The focused chain's One-click switch: a click on a bid or ask sends a limit there.
    ChainOneClick,
    /// The focused option chain's chart beside it, on one price scale (decision 0029).
    ChainChart,
    ChainPnl,
    /// Write the journal's trades as CSV to the config folder.
    ExportJournal,
    /// Record every live feed (and live level sources) to the data folder's `recordings/`
    /// (`crate::recording`). No key: the only free one sits beside Ctrl+Alt+R (Reverse).
    StartRecording,
    StopRecording,
    /// The focused journal: edit the selected trade's tags and note.
    JournalEdit,
    /// The focused journal: stats for all dates or the current trade date.
    JournalAllDates,
    /// The focused journal: open the selected trade's trade snapshot in the OS's default app.
    JournalOpenChart,
    /// The focused journal: list trades, or P&L by account or by tag.
    JournalGroup(crate::wsdoc::JournalGroup),
    /// The focused chart's studies menu (VWAPs, volume profile, levels; templates).
    Studies,
    /// Trading on the focused DOM or chart (roadmap 7): its account and contract.
    Trade(TradeCmd),
    /// The app-wide P&L display: the next mode (off, ticks, currency, both).
    CyclePnl,
    /// The Settings window (decision 0021; Ctrl+,).
    Settings,
    /// The Settings window at a section: a pane menu's "Settings…" (`pane_menu::settings_section`).
    /// Menu only; its key hint is Settings' own.
    SettingsAt(crate::settings_schema::Section),
    /// One setting from the schema (`crate::settings_schema`): set a value, or open the
    /// Settings window at it.
    Setting(crate::settings_schema::SettingCmd),
    /// The app-wide P&L display: this mode.
    SetPnl(crate::pnl::PnlDisplay),
    /// The focused chart's timeframe (roadmap 9). No key: typed on the chart (`5`, `1h` +
    /// Enter), the header chip, the pane menu and the palette.
    Timeframe(Timeframe),
    /// The focused chart's timeframe list.
    TimeframeMenu,
    /// The focused chart's tick bars: run across sessions, or restart at each session open
    /// (the default). Saved with the chart.
    TickBarsContinuous,
    /// Drawings on the focused chart (roadmap 11; `crate::chart_draw`).
    Draw(DrawCmd),
    /// Strategies (decision 0025; `crate::strategy_view`).
    Strategy(StrategyCmd),
    /// The app-wide UI zoom (`crate::zoom`): the next step up or down, or 100 %. Never the
    /// chart's time or price scale.
    ZoomIn,
    ZoomOut,
    ZoomReset,
    /// One working order on the focused chart, from its tag's menu (`crate::pane_menu`). Not
    /// in the palette or the keymap: it needs the order under the pointer.
    Order(OrderCmd),
    /// The replay bar (`crate::replay_bar`). Its plain keys act only while a replay plays and
    /// the focused pane doesn't use the key itself.
    Replay(ReplayCmd),
}

/// The replay bar's controls (`crate::replay_bar`). Every jump ends paused at its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayCmd {
    PlayPause,
    /// One bar of the focused chart (1 minute when it has no time bars).
    StepBack,
    StepForward,
    /// The previous or next session open.
    PrevSession,
    NextSession,
    Slower,
    Faster,
    /// A date and time in the display time zone.
    GoTo,
}

impl ReplayCmd {
    pub const ALL: [ReplayCmd; 8] = [
        ReplayCmd::PlayPause,
        ReplayCmd::StepBack,
        ReplayCmd::StepForward,
        ReplayCmd::PrevSession,
        ReplayCmd::NextSession,
        ReplayCmd::Slower,
        ReplayCmd::Faster,
        ReplayCmd::GoTo,
    ];
}

/// What an order's tag menu does: the same paths as its tag's controls and the DOM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderCmd {
    /// Move the position's stops to breakeven (`TradeCmd::Breakeven`).
    Breakeven,
    /// Type a new size for this order (the tag's size field).
    Resize(u64),
    /// Cancel this order (the tag's ×).
    Cancel(u64),
    /// Place a limit or stop at the price right-clicked (the chart menu's order section).
    /// The contract is the one the row was offered on: a chart that switched since refuses it.
    Place(crate::chart_trade::MenuOrder, crate::chart_trade::Offer),
    /// Stage that order on the chart with its bracket's legs, unsent until Place (the order
    /// section's "…with bracket" row; `chart_trade::stage`).
    Stage(crate::chart_trade::MenuOrder, crate::chart_trade::Offer),
}

/// Strategy controls. Stop all acts on every instance from anywhere and is never gated;
/// Start and Stop act on the strategy panel's selected instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrategyCmd {
    /// Stop every strategy and flatten their positions (decision 0025's default).
    StopAll,
    /// Stop every strategy; protective exits stay and the positions become the trader's.
    StopAllKeep,
    /// Show the strategy panel: focus it, else open one in a new tab.
    Open,
    /// Start (arm) the selected strategy. Never from a key: starting is always deliberate.
    Start,
    /// Stop the selected strategy and flatten its position.
    Stop,
    /// The focused chart: show or hide strategy markers (entries, exits, skipped signals).
    ToggleMarks,
}

impl StrategyCmd {
    pub const ALL: [StrategyCmd; 6] = [
        StrategyCmd::StopAll,
        StrategyCmd::StopAllKeep,
        StrategyCmd::Open,
        StrategyCmd::Start,
        StrategyCmd::Stop,
        StrategyCmd::ToggleMarks,
    ];
}

/// Drawing actions on the focused chart. The drawings belong to its product: every chart of
/// it shows them, and undo/redo act on the product's drawings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrawCmd {
    /// Enter drawing mode with this tool (again: leave it).
    Tool(Tool),
    Undo,
    Redo,
    DeleteSelected,
    /// Show or hide drawings on this chart (saved with the pane).
    ToggleVisible,
    /// Delete every drawing of the chart's product (asks first; undo brings them back).
    DeleteAll,
    /// The selected drawing's (or new drawings') color, weight, dash.
    NextColor,
    ToggleWidth,
    ToggleDashed,
    /// Keep the temporary measure as a drawing.
    PinMeasure,
    /// Open the style editor on the selected drawing (decision 0026).
    Style,
    /// Show or hide one part of the measure's readout (its right-click menu; app-wide).
    MeasurePart(crate::chart_draw::MeasurePart),
    /// Remove the temporary measure.
    RemoveMeasure,
}

impl DrawCmd {
    pub const ALL: [DrawCmd; 18] = [
        DrawCmd::Tool(Tool::HLine),
        DrawCmd::Tool(Tool::HRay),
        DrawCmd::Tool(Tool::Trend),
        DrawCmd::Tool(Tool::Ray),
        DrawCmd::Tool(Tool::Rect),
        DrawCmd::Tool(Tool::VLine),
        DrawCmd::Tool(Tool::Note),
        DrawCmd::Tool(Tool::Measure),
        DrawCmd::PinMeasure,
        DrawCmd::Undo,
        DrawCmd::Redo,
        DrawCmd::DeleteSelected,
        DrawCmd::ToggleVisible,
        DrawCmd::DeleteAll,
        DrawCmd::NextColor,
        DrawCmd::ToggleWidth,
        DrawCmd::ToggleDashed,
        DrawCmd::Style,
    ];
}

/// Trading actions of the focused pane (`crate::trade_bar::run`). Flatten and Cancel All act
/// on the pane's contract and are never blocked by arming, risk or prop rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TradeCmd {
    /// Cancel the contract's orders and close its position at market.
    Flatten,
    /// Cancel the contract's working orders.
    CancelAll,
    /// Enable or disable one-click trading on the DOM ("Trading: Enabled / Disabled").
    ToggleArm,
    /// Enable DOM trading (sticky: it stays on until turned off).
    Enable,
    /// Disable DOM trading.
    Disable,
    /// Move the position's stop to breakeven (the average price, on the safe side of the
    /// tick grid), through the order-modify path.
    Breakeven,
    /// Show bracket distances in ticks or points (per account).
    BracketUnits,
    /// Edit the bracket presets in the DOM's panel.
    EditBrackets,
    /// One bracket for the whole position, or a bracket per entry (per account and product).
    BracketMode,
    /// Reset the pane's paper account (second press confirms; `crate::paper_reset`).
    ResetPaper,
    /// The same, clearing the account's journal too.
    ResetPaperJournal,
    /// Trade on the next account.
    NextAccount,
    QtyUp,
    QtyDown,
    /// Pick a size preset (`dom_trade::QTY_PRESETS`, by index).
    QtyPreset(u8),
    /// Type a size: digits, then Enter; Esc cancels.
    TypeQty,
    /// Cycle the bracket preset (none, 8/16, 12/24, 20/40 ticks).
    NextBracket,
    /// The DOM header's order buttons (`dom_trade::QuickOrder`): gated by arming like ladder
    /// clicks.
    BuyMarket,
    SellMarket,
    JoinBid,
    JoinAsk,
    Reverse,
    /// A buy limit at the best ask, and a sell limit at the best bid (order keys only).
    BuyAsk,
    SellBid,
    /// The DOM's order hotkeys ("Keys on / off"): off by default, sticky, saved per pane.
    /// Order keys need it as well as the trading switch; buttons and the palette don't.
    ToggleHotkeys,
}

impl TradeCmd {
    pub const ALL: [TradeCmd; 22] = [
        TradeCmd::Flatten,
        TradeCmd::CancelAll,
        TradeCmd::ToggleArm,
        TradeCmd::NextAccount,
        TradeCmd::QtyUp,
        TradeCmd::QtyDown,
        TradeCmd::QtyPreset(0),
        TradeCmd::QtyPreset(1),
        TradeCmd::QtyPreset(2),
        TradeCmd::QtyPreset(3),
        TradeCmd::QtyPreset(4),
        TradeCmd::TypeQty,
        TradeCmd::NextBracket,
        TradeCmd::BuyMarket,
        TradeCmd::SellMarket,
        TradeCmd::JoinBid,
        TradeCmd::JoinAsk,
        TradeCmd::Reverse,
        TradeCmd::Breakeven,
        TradeCmd::BuyAsk,
        TradeCmd::SellBid,
        TradeCmd::ToggleHotkeys,
    ];

    /// Trading commands with no key of their own (palette and the DOM's panel).
    pub const PALETTE_ONLY: [TradeCmd; 7] = [
        TradeCmd::Enable,
        TradeCmd::Disable,
        TradeCmd::BracketUnits,
        TradeCmd::EditBrackets,
        TradeCmd::BracketMode,
        TradeCmd::ResetPaper,
        TradeCmd::ResetPaperJournal,
    ];

    /// The order a header button sends, for the order commands.
    pub fn quick(self) -> Option<crate::dom_trade::QuickOrder> {
        use crate::dom_trade::QuickOrder as Q;
        Some(match self {
            TradeCmd::BuyMarket => Q::BuyMarket,
            TradeCmd::SellMarket => Q::SellMarket,
            TradeCmd::JoinBid => Q::JoinBid,
            TradeCmd::JoinAsk => Q::JoinAsk,
            TradeCmd::Reverse => Q::Reverse,
            TradeCmd::BuyAsk => Q::BuyAsk,
            TradeCmd::SellBid => Q::SellBid,
            _ => return None,
        })
    }

    /// Whether this command, sent from a key, needs the DOM's order hotkeys on: every key that
    /// places an order. Flatten and Cancel all never do (exits are never blocked), nor do
    /// arming, quantity, bracket, account and BE keys (they place nothing new).
    pub fn needs_hotkeys(self) -> bool {
        self.quick().is_some()
    }
}

/// A key binding: modifiers plus GPUI's key name.
pub struct Binding {
    pub cmd: Command,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: &'static str,
}

const fn b(cmd: Command, ctrl: bool, shift: bool, alt: bool, key: &'static str) -> Binding {
    Binding {
        cmd,
        ctrl,
        shift,
        alt,
        key,
    }
}

/// Every key binding (the first one for a command is its hint). Focus (Alt+arrows) and tab
/// selection (Ctrl+1..9) are patterns, handled in [`command_for`].
pub const BINDINGS: &[Binding] = &[
    b(Command::Settings, true, false, false, ","),
    b(Command::Split(Axis::Row), true, true, false, "right"),
    b(Command::Split(Axis::Column), true, true, false, "down"),
    b(Command::ClosePane, true, false, false, "w"),
    b(Command::Toggle(PaneKind::Dom), true, true, false, "d"),
    b(Command::Toggle(PaneKind::Tape), true, true, false, "e"),
    b(Command::ChartOnly, true, true, false, "c"),
    b(Command::ShowHidden, true, true, false, "h"),
    b(Command::PopOut, true, true, false, "o"),
    b(Command::DockBesideChart, true, true, false, "b"),
    b(Command::NewTab, true, false, false, "t"),
    b(Command::CloseTab, true, false, false, "f4"),
    b(Command::CloseTab, true, true, false, "w"),
    b(Command::ReopenTab, true, true, false, "t"),
    b(Command::NextTab, true, false, false, "tab"),
    b(Command::NextTab, true, false, false, "pagedown"),
    b(Command::PrevTab, true, true, false, "tab"),
    b(Command::PrevTab, true, false, false, "pageup"),
    b(Command::TabToNewWindow, true, true, false, "n"),
    b(Command::CycleLink, true, false, false, "l"),
    b(Command::ToggleCrosshairSync, true, true, false, "x"),
    b(Command::ToggleMagnet, true, true, false, "g"),
    b(Command::Menu, false, true, false, "f10"),
    b(Command::Menu, true, false, false, "."),
    b(Command::Palette, true, true, false, "p"),
    b(Command::SaveAs, true, true, false, "s"),
    b(Command::Open, true, false, false, "o"),
    // Ctrl+Shift+J: every open position, on every account.
    b(Command::OpenPositions, true, true, false, "j"),
    b(Command::ToggleStudyLabels, true, true, false, "l"),
    b(Command::ToggleStudies, true, true, false, "i"),
    b(Command::CleanChart, true, true, false, "m"),
    b(Command::ShowKeys, false, false, false, "f1"),
    b(Command::Studies, true, false, false, "i"),
    // Drawings (roadmap 11). Alt+letter on charts; Alt+V/R/B/G stay the volume profile's.
    b(
        Command::Draw(DrawCmd::Tool(Tool::HLine)),
        false,
        false,
        true,
        "h",
    ),
    b(
        Command::Draw(DrawCmd::Tool(Tool::HRay)),
        false,
        true,
        true,
        "h",
    ),
    b(
        Command::Draw(DrawCmd::Tool(Tool::Trend)),
        false,
        false,
        true,
        "t",
    ),
    b(
        Command::Draw(DrawCmd::Tool(Tool::Ray)),
        false,
        true,
        true,
        "t",
    ),
    // Alt+E: trade history on the chart ("executions"; Alt+T is the trend line's).
    b(Command::ToggleTrades, false, false, true, "e"),
    b(
        Command::Draw(DrawCmd::Tool(Tool::Rect)),
        false,
        false,
        true,
        "z",
    ),
    b(
        Command::Draw(DrawCmd::Tool(Tool::VLine)),
        false,
        false,
        true,
        "l",
    ),
    b(
        Command::Draw(DrawCmd::Tool(Tool::Note)),
        false,
        false,
        true,
        "n",
    ),
    // Alt+U (ruler): Alt+M stays the symbol picker's micro/mini toggle.
    b(
        Command::Draw(DrawCmd::Tool(Tool::Measure)),
        false,
        false,
        true,
        "u",
    ),
    // Alt+S: the selected drawing's style editor.
    b(Command::Draw(DrawCmd::Style), false, false, true, "s"),
    b(Command::Draw(DrawCmd::Undo), true, false, false, "z"),
    b(Command::Draw(DrawCmd::Redo), true, false, false, "y"),
    b(Command::Draw(DrawCmd::Redo), true, true, false, "z"),
    // Trading (roadmap 7): always Ctrl+Shift or Ctrl+Alt, so no stray key can trade.
    b(Command::Trade(TradeCmd::Flatten), true, true, false, "f"),
    b(Command::Trade(TradeCmd::CancelAll), true, true, false, "a"),
    b(
        Command::Trade(TradeCmd::ToggleArm),
        true,
        true,
        false,
        "space",
    ),
    b(
        Command::Trade(TradeCmd::NextAccount),
        true,
        true,
        false,
        "u",
    ),
    b(Command::CyclePnl, true, false, true, "p"),
    // The replay bar: plain keys, passed on to the panes when no replay plays (and to a
    // focused pane that uses them: the chain's Left/Right, typed text).
    b(
        Command::Replay(ReplayCmd::PlayPause),
        false,
        false,
        false,
        "space",
    ),
    b(
        Command::Replay(ReplayCmd::StepBack),
        false,
        false,
        false,
        "left",
    ),
    b(
        Command::Replay(ReplayCmd::StepForward),
        false,
        false,
        false,
        "right",
    ),
    b(
        Command::Replay(ReplayCmd::PrevSession),
        false,
        true,
        false,
        "left",
    ),
    b(
        Command::Replay(ReplayCmd::NextSession),
        false,
        true,
        false,
        "right",
    ),
    b(Command::Replay(ReplayCmd::Slower), false, false, false, "["),
    b(Command::Replay(ReplayCmd::Faster), false, false, false, "]"),
    // Ctrl+= and Ctrl++ (Shift+= on most layouts, or the keypad's +), Ctrl+−, Ctrl+0.
    b(Command::ZoomIn, true, false, false, "="),
    b(Command::ZoomIn, true, true, false, "="),
    b(Command::ZoomIn, true, false, false, "+"),
    b(Command::ZoomIn, true, true, false, "+"),
    b(Command::ZoomOut, true, false, false, "-"),
    b(Command::ZoomReset, true, false, false, "0"),
    // Strategies (decision 0025): Stop all, from any pane.
    b(
        Command::Strategy(StrategyCmd::StopAll),
        true,
        true,
        false,
        "r",
    ),
    // Header order buttons: Ctrl+Alt, never a bare or Shift-only key.
    b(Command::Trade(TradeCmd::BuyMarket), true, false, true, "b"),
    b(Command::Trade(TradeCmd::SellMarket), true, false, true, "s"),
    b(Command::Trade(TradeCmd::JoinBid), true, false, true, "j"),
    b(Command::Trade(TradeCmd::JoinAsk), true, false, true, "k"),
    b(Command::Trade(TradeCmd::Reverse), true, false, true, "r"),
    // O for the offer; I left of it, as the bid column is left of the ask. Neither is one
    // modifier from an emergency key (Cancel All Ctrl+Shift+A, Flatten Ctrl+Shift+F) or
    // Undo, nor a macOS system chord with Cmd+Option.
    b(Command::Trade(TradeCmd::BuyAsk), true, false, true, "o"),
    b(Command::Trade(TradeCmd::SellBid), true, false, true, "i"),
    b(
        Command::Trade(TradeCmd::ToggleHotkeys),
        true,
        true,
        false,
        "y",
    ),
    b(Command::Trade(TradeCmd::Breakeven), true, false, true, "e"),
    b(Command::Trade(TradeCmd::QtyUp), true, false, true, "up"),
    b(Command::Trade(TradeCmd::QtyDown), true, false, true, "down"),
    // The size: Ctrl+Alt as every trading key (Alt alone is focus movement and types characters
    // on macOS; Cmd+Shift+3/4/5 are macOS screenshots). T for type: Cmd+Option+Q would be one
    // slipped modifier from Cmd+Q, a slip on T only opens a tab.
    b(
        Command::Trade(TradeCmd::QtyPreset(0)),
        true,
        false,
        true,
        "1",
    ),
    b(
        Command::Trade(TradeCmd::QtyPreset(1)),
        true,
        false,
        true,
        "2",
    ),
    b(
        Command::Trade(TradeCmd::QtyPreset(2)),
        true,
        false,
        true,
        "3",
    ),
    b(
        Command::Trade(TradeCmd::QtyPreset(3)),
        true,
        false,
        true,
        "4",
    ),
    b(
        Command::Trade(TradeCmd::QtyPreset(4)),
        true,
        false,
        true,
        "5",
    ),
    b(Command::Trade(TradeCmd::TypeQty), true, false, true, "t"),
    b(
        Command::Trade(TradeCmd::NextBracket),
        true,
        true,
        false,
        "k",
    ),
];

/// The command bound to a keystroke, if any.
pub fn command_for(k: &Keystroke) -> Option<Command> {
    let m = k.modifiers;
    // GPUI's `secondary`: Cmd on macOS, Ctrl elsewhere.
    let mac = cfg!(target_os = "macos");
    let ctrl = if mac { m.platform } else { m.control };
    // The modifier that isn't "Ctrl" on this platform (Ctrl on macOS, Win elsewhere) is never
    // part of a binding.
    if (mac && m.control) || (!mac && m.platform) {
        return None;
    }
    let key = k.key.as_str();
    // AltGr arrives as Ctrl+Alt on Windows/Linux: when it types a character other than the
    // key itself ("ś" from AltGr+S on a Polish layout), it is text, never a command (several
    // Ctrl+Alt keys send orders).
    if !mac
        && m.control
        && m.alt
        && k.key_char
            .as_deref()
            .is_some_and(|c| !c.is_empty() && !c.eq_ignore_ascii_case(key))
    {
        return None;
    }
    if !ctrl && !m.shift && m.alt {
        let dir = match key {
            "left" => Some(Direction::Left),
            "right" => Some(Direction::Right),
            "up" => Some(Direction::Up),
            "down" => Some(Direction::Down),
            _ => None,
        };
        if let Some(d) = dir {
            return Some(Command::Focus(d));
        }
    }
    if ctrl && !m.shift && !m.alt && key.len() == 1 && (b'1'..=b'9').contains(&key.as_bytes()[0]) {
        return Some(Command::SelectTab((key.as_bytes()[0] - b'1') as usize));
    }
    BINDINGS
        .iter()
        .find(|b| b.ctrl == ctrl && b.shift == m.shift && b.alt == m.alt && b.key == key)
        .map(|b| b.cmd)
}

/// Whether holding the key down repeats `cmd`. Never for arming or anything that sends or
/// cancels orders: a held Ctrl+Alt+B must send one order, not one per key repeat.
pub fn repeats(cmd: Command) -> bool {
    // Nor play/pause: a held Space would flip it on every repeat.
    !matches!(
        cmd,
        Command::Trade(_) | Command::Strategy(_) | Command::Replay(ReplayCmd::PlayPause)
    )
}

/// The keys of a command's first binding, one per cap (`keycap`): modifiers first, in the
/// platform's order (macOS: ⌥ ⇧ ⌘; elsewhere Ctrl Alt Shift), then the key. Empty when the
/// command has no key.
pub fn keys(c: Command) -> Vec<String> {
    keys_on(c, cfg!(target_os = "macos"))
}

fn keys_on(c: Command, mac: bool) -> Vec<String> {
    let mods = |ctrl: bool, alt: bool, shift: bool| -> Vec<String> {
        let all: [(bool, &str); 3] = if mac {
            [(alt, "⌥"), (shift, "⇧"), (ctrl, "⌘")]
        } else {
            [(ctrl, "Ctrl"), (alt, "Alt"), (shift, "Shift")]
        };
        all.iter()
            .filter(|(on, _)| *on)
            .map(|(_, k)| (*k).to_owned())
            .collect()
    };
    match c {
        Command::Focus(_) => {
            let mut v = mods(false, true, false);
            v.push("arrows".to_owned());
            return v;
        }
        // The same window at a section: Settings' key.
        Command::SettingsAt(_) => return keys_on(Command::Settings, mac),
        Command::SelectTab(i) if i < 9 => {
            let mut v = mods(true, false, false);
            v.push((i + 1).to_string());
            return v;
        }
        _ => {}
    }
    let Some(b) = BINDINGS.iter().find(|b| b.cmd == c) else {
        return Vec::new();
    };
    let key = match b.key {
        "right" => "→".to_owned(),
        "left" => "←".to_owned(),
        "down" => "↓".to_owned(),
        "tab" => "Tab".to_owned(),
        "pagedown" => "PgDn".to_owned(),
        "pageup" => "PgUp".to_owned(),
        "up" => "↑".to_owned(),
        "space" => "Space".to_owned(),
        // A lone comma reads as punctuation in a row's hint: named, as macOS shows "⌘,".
        "," if !mac => "Comma".to_owned(),
        k => k.to_uppercase(),
    };
    let mut v = mods(b.ctrl, b.alt, b.shift);
    v.push(key);
    v
}

/// A command's key as text ("Ctrl+Shift+F", "⇧⌘F"): for sentences ("Ctrl+Z undoes") and
/// search. Menus, tooltips, the palette and Settings › Keys draw [`keys`] as caps instead.
pub fn hint(c: Command) -> String {
    hint_on(c, cfg!(target_os = "macos"))
}

fn hint_on(c: Command, mac: bool) -> String {
    keys_on(c, mac).join(if mac { "" } else { "+" })
}

/// What a command is called in menus and the palette.
pub fn label(c: Command) -> String {
    match c {
        Command::Focus(d) => format!("Focus pane {}", format!("{d:?}").to_lowercase()),
        Command::Split(Axis::Row) => "Split right".into(),
        Command::Split(Axis::Column) => "Split down".into(),
        Command::ClosePane => "Close pane".into(),
        Command::SetKind(k) => format!("Change pane to {}", k.label()),
        Command::CycleLink => "Next link group".into(),
        Command::SetLink(None) => "Unlink pane".into(),
        Command::SetLink(l) => format!("Link pane to group {}", link_letter(l)),
        Command::Preset(p) => format!("Layout: {}", p.label()),
        Command::HidePane => "Hide pane".into(),
        Command::Toggle(PaneKind::Dom) => "Hide/show DOM".into(),
        Command::Toggle(PaneKind::Tape) => "Hide/show time & sales".into(),
        Command::Toggle(PaneKind::Chart) => "Hide/show chart".into(),
        Command::Toggle(PaneKind::Journal) => "Hide/show journal".into(),
        Command::Toggle(PaneKind::Strategies) => "Hide/show strategies".into(),
        Command::Toggle(PaneKind::Chain) => "Hide/show option chain".into(),
        Command::Toggle(PaneKind::Positions) => "Hide/show positions".into(),
        Command::ChartOnly => "Chart only (hide/restore linked DOM and T&S)".into(),
        Command::ShowHidden => "Show hidden panes".into(),
        Command::PopOut => "Pop out pane into its own window".into(),
        Command::Reattach => "Re-attach detached panes".into(),
        Command::DockBesideChart => "Dock beside focused chart".into(),
        Command::NewTab => "New tab".into(),
        Command::CloseTab => "Close tab".into(),
        Command::RenameTab => "Rename tab…".into(),
        Command::TogglePinTab => "Pin tab".into(),
        Command::CloseOtherTabs => "Close other tabs".into(),
        Command::ReopenTab => "Reopen closed tab".into(),
        Command::NextTab => "Next tab".into(),
        Command::PrevTab => "Previous tab".into(),
        Command::SelectTab(i) => format!("Go to tab {}", i + 1),
        Command::TabToNewWindow => "Move tab to new window".into(),
        Command::ToggleCrosshairSync => "Crosshair sync on/off (within link groups)".into(),
        Command::ToggleCrosshairSyncPrice => {
            "Crosshair sync: price too, same product (on/off)".into()
        }
        Command::ToggleMagnet => "Chart: magnet crosshair, snap to OHLC (on/off)".into(),
        Command::ToggleOrderStrip => {
            "Chart: order strip, market and limit at the crosshair (show/hide)".into()
        }
        Command::Menu => "Pane menu".into(),
        Command::Palette => "Command palette".into(),
        Command::SaveAs => "Save workspace as…".into(),
        Command::Open => "Open workspace…".into(),
        Command::RestartToUpdate => "Restart to update".into(),
        Command::RollBackUpdate => "Roll back the last update".into(),
        Command::ResetPanels => "Reset pop-up panel positions".into(),
        Command::ExportDiagnostics => "Export diagnostics…".into(),
        Command::DownloadDays => "Download days…".into(),
        Command::ToggleStudyLabels => "Chart: hide/show study labels".into(),
        Command::ToggleStudies => "Chart: hide/show studies".into(),
        Command::CleanChart => "Clean chart (no study labels or trades)".into(),
        Command::ShowKeys => "Keyboard shortcuts…".into(),
        Command::ToggleTrades => "Show trades on the chart".into(),
        Command::TradesOnAllCharts(false) => "Trades: hide on all charts".into(),
        Command::TradesOnAllCharts(true) => "Trades: show on all charts".into(),
        Command::ToggleTradeLines => "Trades: entry–exit lines".into(),
        Command::ToggleTradeLabels => "Trades: labels".into(),
        Command::TradeAccounts(true) => "Trades: all accounts".into(),
        Command::TradeAccounts(false) => "Trades: this pane's account".into(),
        Command::TradePeriod(p) => format!("Trades: {}", p.label().to_lowercase()),
        Command::FooterLink(k) => k.name().into(),
        Command::Columns(ColumnCmd::Toggle(c)) => format!("Show/hide {}", c.label()),
        Command::Columns(ColumnCmd::AutoFit(c)) => format!("Fit {} to content", c.label()),
        Command::Columns(ColumnCmd::AutoFitAll) => "Fit columns to content".into(),
        Command::Columns(ColumnCmd::Reset) => "Reset columns".into(),
        Command::ShowSourceHere => "Show the source's symbol in this pane".into(),
        Command::ShowSourceInTab => "Switch all unavailable panes to the source's symbol".into(),
        Command::Columns(ColumnCmd::Width(c, w)) => format!("Set {} width to {w} px", c.label()),
        Command::ToggleLevels => "Chart: hide/show levels".into(),
        Command::ToggleLevelLabels => "Chart: hide/show level labels".into(),
        Command::TogglePerfStats => "Performance stats on/off (chart headers)".into(),
        Command::ToggleLatencyHud => "Latency HUD on/off (chart headers)".into(),
        Command::DomTrail => "DOM: price trail on/off".into(),
        Command::DomTrailHalfLife => "DOM: price trail half-life (next)".into(),
        Command::OpenJournal => "Open journal".into(),
        Command::OpenChain => "Open option chain".into(),
        Command::OpenPositions => "Open positions".into(),
        Command::StartRecording => "Start recording".into(),
        Command::StopRecording => "Stop recording".into(),
        Command::ChainOneClick => "Option chain: One-click on/off".into(),
        Command::ChainChart => "Option chain: chart beside the chain on/off".into(),
        Command::ChainPnl => "Option chain: P&L to close on/off".into(),
        Command::ExportJournal => "Export journal (CSV)".into(),
        Command::JournalEdit => "Journal: edit tags and note".into(),
        Command::JournalAllDates => "Journal: stats for all dates / today".into(),
        Command::JournalOpenChart => "Journal: open the selected trade's snapshot".into(),
        Command::JournalGroup(g) => match g {
            crate::wsdoc::JournalGroup::Trades => "Journal: list trades",
            crate::wsdoc::JournalGroup::Accounts => "Journal: P&L by account",
            crate::wsdoc::JournalGroup::Tags => "Journal: P&L by tag",
        }
        .into(),
        Command::Studies => "Chart: studies (VWAP, volume profile, levels, templates)…".into(),
        Command::CyclePnl => "P&L display: next (off, ticks, currency, both)".into(),
        Command::SetPnl(m) => format!("P&L display: {}", m.label()),
        Command::Settings | Command::SettingsAt(_) => "Settings…".into(),
        Command::ZoomIn => "Zoom in".into(),
        Command::ZoomOut => "Zoom out".into(),
        Command::ZoomReset => "Reset zoom".into(),
        Command::Setting(c) => c.label(),
        Command::Timeframe(tf) if tf.bar_kind().is_some() => {
            format!("Timeframe: {} ({tf})", crate::pane::timeframe_name(tf))
        }
        Command::Timeframe(tf) => format!("Timeframe: {tf}"),
        Command::TimeframeMenu => "Chart: timeframe…".into(),
        Command::TickBarsContinuous => {
            "Chart: tick, volume, range and seconds bars run continuously, no session reset \
             (on/off)"
                .into()
        }
        Command::Draw(d) => match d {
            DrawCmd::Tool(t) => format!("Drawings: {}", t.label()),
            DrawCmd::Undo => "Drawings: undo".into(),
            DrawCmd::Redo => "Drawings: redo".into(),
            DrawCmd::DeleteSelected => "Drawings: delete selected".into(),
            DrawCmd::ToggleVisible => "Drawings: show/hide on this chart".into(),
            DrawCmd::DeleteAll => "Drawings: delete all for this chart's product…".into(),
            DrawCmd::NextColor => "Drawings: next color".into(),
            DrawCmd::ToggleWidth => "Drawings: line width (next, 1-4 px)".into(),
            DrawCmd::ToggleDashed => "Drawings: dashed on/off".into(),
            DrawCmd::Style => "Drawings: style of the selected drawing…".into(),
            DrawCmd::PinMeasure => "Drawings: pin measure".into(),
            DrawCmd::MeasurePart(p) => format!("Measure: show {}", p.label()),
            DrawCmd::RemoveMeasure => "Measure: remove".into(),
        },
        Command::Strategy(c) => match c {
            StrategyCmd::StopAll => "Strategies: Stop all (flatten their positions)",
            StrategyCmd::StopAllKeep => "Strategies: Stop all, keep positions",
            StrategyCmd::Open => "Open strategies",
            StrategyCmd::Start => "Strategies: start the selected strategy (paper)",
            StrategyCmd::Stop => "Strategies: stop the selected strategy (flatten)",
            StrategyCmd::ToggleMarks => "Chart: show/hide strategy markers",
        }
        .into(),
        Command::Replay(r) => match r {
            ReplayCmd::PlayPause => "Replay: play / pause",
            ReplayCmd::StepBack => "Replay: back one bar",
            ReplayCmd::StepForward => "Replay: forward one bar",
            ReplayCmd::PrevSession => "Replay: previous session open",
            ReplayCmd::NextSession => "Replay: next session open",
            ReplayCmd::Slower => "Replay: slower",
            ReplayCmd::Faster => "Replay: faster",
            ReplayCmd::GoTo => "Replay: go to…",
        }
        .into(),
        Command::Order(o) => match o {
            OrderCmd::Breakeven => "Move to breakeven",
            OrderCmd::Resize(_) => "Change size…",
            OrderCmd::Cancel(_) => "Cancel order",
            OrderCmd::Place(..) => "Place order",
            OrderCmd::Stage(..) => "Stage order with bracket",
        }
        .into(),
        Command::Trade(t) => match t {
            TradeCmd::Flatten => "Trade: Flatten (cancel orders, close position)",
            TradeCmd::CancelAll => "Trade: Cancel all orders",
            TradeCmd::ToggleArm => "Trade: toggle (enable/disable one-click DOM trading)",
            TradeCmd::Enable => "Trade: enable",
            TradeCmd::Disable => "Trade: disable",
            TradeCmd::Breakeven => "Trade: move the stop to breakeven",
            TradeCmd::BracketUnits => "Trade: bracket units (ticks / points)",
            TradeCmd::EditBrackets => "Trade: edit bracket presets…",
            TradeCmd::BracketMode => "Trade: bracket for the whole position / per entry (switch)",
            TradeCmd::ResetPaper => "Trade: Reset paper account (keeps the journal)",
            TradeCmd::ResetPaperJournal => "Trade: Reset paper account and clear its journal",
            TradeCmd::NextAccount => "Trade: Next account",
            TradeCmd::QtyUp => "Trade: Quantity up",
            TradeCmd::QtyDown => "Trade: Quantity down",
            TradeCmd::QtyPreset(0) => "Trade: Quantity preset 1",
            TradeCmd::QtyPreset(1) => "Trade: Quantity preset 2",
            TradeCmd::QtyPreset(2) => "Trade: Quantity preset 3",
            TradeCmd::QtyPreset(3) => "Trade: Quantity preset 4",
            TradeCmd::QtyPreset(_) => "Trade: Quantity preset 5",
            TradeCmd::TypeQty => "Trade: Type a quantity",
            TradeCmd::NextBracket => "Trade: Next bracket preset",
            TradeCmd::BuyMarket => "Trade: Buy market (selected quantity)",
            TradeCmd::SellMarket => "Trade: Sell market (selected quantity)",
            TradeCmd::JoinBid => "Trade: Join bid (buy limit at the best bid)",
            TradeCmd::JoinAsk => "Trade: Join ask (sell limit at the best ask)",
            TradeCmd::Reverse => "Trade: Reverse position (close it, open the other side)",
            TradeCmd::BuyAsk => "Trade: Buy at the ask (limit at the best ask)",
            TradeCmd::SellBid => "Trade: Sell at the bid (limit at the best bid)",
            TradeCmd::ToggleHotkeys => "Trade: toggle order hotkeys (Keys on/off)",
        }
        .into(),
    }
}

/// Every command the palette offers (it runs on the focused pane, tab and window).
pub fn palette() -> Vec<Command> {
    let mut v = vec![
        Command::Split(Axis::Row),
        Command::Split(Axis::Column),
        Command::ClosePane,
        Command::Toggle(PaneKind::Dom),
        Command::Toggle(PaneKind::Tape),
        Command::ChartOnly,
        Command::HidePane,
        Command::ShowHidden,
        Command::PopOut,
        Command::Reattach,
        Command::NewTab,
        Command::CloseTab,
        Command::RenameTab,
        Command::TogglePinTab,
        Command::CloseOtherTabs,
        Command::ReopenTab,
        Command::NextTab,
        Command::PrevTab,
        Command::TabToNewWindow,
        Command::CycleLink,
    ];
    v.extend([None, Some(0), Some(1), Some(2), Some(3)].map(Command::SetLink));
    v.extend(
        [
            PaneKind::Chart,
            PaneKind::Dom,
            PaneKind::Tape,
            PaneKind::Journal,
            PaneKind::Strategies,
            PaneKind::Chain,
            PaneKind::Positions,
        ]
        .map(Command::SetKind),
    );
    v.extend(Preset::ALL.map(Command::Preset));
    v.extend([
        Command::ToggleCrosshairSync,
        Command::ToggleCrosshairSyncPrice,
        Command::ToggleMagnet,
        Command::ToggleOrderStrip,
        Command::ToggleStudyLabels,
        Command::ToggleStudies,
        Command::CleanChart,
        Command::ToggleTrades,
        Command::TradesOnAllCharts(false),
        Command::TradesOnAllCharts(true),
        Command::ToggleTradeLines,
        Command::ToggleTradeLabels,
        Command::TradeAccounts(false),
        Command::TradeAccounts(true),
        Command::TradePeriod(crate::trade_marks::Period::Today),
        Command::TradePeriod(crate::trade_marks::Period::Week),
        Command::TradePeriod(crate::trade_marks::Period::All),
        Command::ShowKeys,
        Command::ToggleLevels,
        Command::ToggleLevelLabels,
        Command::TogglePerfStats,
        Command::ToggleLatencyHud,
        Command::DomTrail,
        Command::DomTrailHalfLife,
        Command::OpenJournal,
        Command::OpenChain,
        Command::OpenPositions,
        Command::ChainOneClick,
        Command::ChainChart,
        Command::ChainPnl,
        Command::ExportJournal,
        Command::StartRecording,
        Command::StopRecording,
        Command::JournalEdit,
        Command::JournalAllDates,
        Command::JournalOpenChart,
        Command::JournalGroup(crate::wsdoc::JournalGroup::Trades),
        Command::JournalGroup(crate::wsdoc::JournalGroup::Accounts),
        Command::JournalGroup(crate::wsdoc::JournalGroup::Tags),
        Command::Studies,
        Command::TimeframeMenu,
        Command::TickBarsContinuous,
        Command::SaveAs,
        Command::Open,
        Command::ResetPanels,
        Command::RestartToUpdate,
        Command::RollBackUpdate,
        Command::Menu,
    ]);
    // Columns of the focused DOM or time & sales.
    v.extend(
        DomCol::ALL
            .map(|c| Command::Columns(ColumnCmd::Toggle(Column::Dom(c))))
            .into_iter()
            .chain(TapeCol::ALL.map(|c| Command::Columns(ColumnCmd::Toggle(Column::Tape(c)))))
            .chain(JournalCol::ALL.map(|c| Command::Columns(ColumnCmd::Toggle(Column::Journal(c)))))
            .chain([
                Command::Columns(ColumnCmd::AutoFitAll),
                Command::Columns(ColumnCmd::Reset),
            ]),
    );
    v.extend([Command::ShowSourceHere, Command::ShowSourceInTab]);
    v.extend(TradeCmd::ALL.map(Command::Trade));
    v.extend(StrategyCmd::ALL.map(Command::Strategy));
    v.extend(TradeCmd::PALETTE_ONLY.map(Command::Trade));
    v.push(Command::CyclePnl);
    v.extend(crate::pnl::PnlDisplay::ALL.map(Command::SetPnl));
    v.extend(ReplayCmd::ALL.map(Command::Replay));
    v.push(Command::ExportDiagnostics);
    v.push(Command::DownloadDays);
    v.extend([Command::ZoomIn, Command::ZoomOut, Command::ZoomReset]);
    v.extend(Timeframe::ALL.map(Command::Timeframe));
    v.extend(DrawCmd::ALL.map(Command::Draw));
    // Settings last: an existing command's words pick it first.
    v.push(Command::Settings);
    v.extend(
        crate::settings_schema::palette()
            .into_iter()
            .map(Command::Setting),
    );
    v
}

/// While the focused pane takes typed text (a symbol search, a note, a timeframe), drawing
/// commands stay with the text: their Alt+letters (on macOS Option types characters, e.g. in
/// a note) and undo/redo. Every other binding runs as before (Alt+arrows, F1, Shift+F10, …).
pub fn runs_while_typing(_k: &Keystroke, cmd: Command) -> bool {
    !matches!(cmd, Command::Draw(_))
}

/// Palette entries whose label holds every word of `query` (any order, any case), in the
/// palette's order. The tests use it to check labels say what traders type; the palette itself
/// ranks with `palette_rank::rank` (fuzzy, run-before first).
#[cfg(test)]
pub fn filter(query: &str, commands: &[Command]) -> Vec<Command> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    commands
        .iter()
        .copied()
        .filter(|c| {
            let l = label(*c).to_lowercase();
            words.iter().all(|w| l.contains(w.as_str()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AltGr is Ctrl+Alt on Windows/Linux: a character it types other than the key itself
    /// ("ś" from AltGr+S, Polish) is text and never sends an order; Ctrl+Alt+S itself (no
    /// character, or the key's own) is still a market sell.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn altgr_text_never_sends_an_order() {
        let mut k = Keystroke::parse("ctrl-alt-s").unwrap();
        assert_eq!(command_for(&k), Some(Command::Trade(TradeCmd::SellMarket)));
        k.key_char = Some("s".into());
        assert_eq!(command_for(&k), Some(Command::Trade(TradeCmd::SellMarket)));
        k.key_char = Some("ś".into());
        assert_eq!(command_for(&k), None);
        let mut b = Keystroke::parse("ctrl-alt-b").unwrap();
        b.key_char = Some("{".into()); // any character other than the key: text
        assert_eq!(command_for(&b), None);
        // Buy at the ask and sell at the bid: AltGr+O types ó (Polish Programmer), AltGr+I types í
        // (layouts with acute vowels on AltGr).
        for (key, ch) in [("ctrl-alt-o", "ó"), ("ctrl-alt-i", "í")] {
            let mut k = Keystroke::parse(key).unwrap();
            assert!(command_for(&k).is_some(), "{key}");
            k.key_char = Some(ch.into());
            assert_eq!(command_for(&k), None, "{key} typing {ch}");
        }
    }

    /// Review round 3: key repeat must never repeat an order (or flip arming back and forth).
    /// The size keys: Ctrl+Alt (Cmd+Option on macOS), as every trading key. Alt alone stays focus
    /// movement and never sets a size. They send nothing, so they need no switch or Keys on.
    #[test]
    fn the_size_keys_are_ctrl_alt() {
        for (i, key) in ["1", "2", "3", "4", "5"].into_iter().enumerate() {
            let preset = Some(Command::Trade(TradeCmd::QtyPreset(i as u8)));
            assert_eq!(cmd(&format!("{}-alt-{key}", ctrl())), preset, "{key}");
            assert_ne!(cmd(&format!("alt-{key}")), preset, "Alt alone: {key}");
        }
        assert_eq!(
            cmd(&format!("{}-alt-t", ctrl())),
            Some(Command::Trade(TradeCmd::TypeQty))
        );
        for tc in [TradeCmd::QtyUp, TradeCmd::QtyPreset(0), TradeCmd::TypeQty] {
            assert!(crate::trade_bar::chart_runs(tc), "{tc:?}");
            assert!(!tc.needs_hotkeys(), "{tc:?}: no order is sent");
        }
    }

    /// AltGr (Ctrl+Alt on Windows' European layouts) that types a character stays text.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn altgr_text_never_sets_a_size() {
        for (key, ch) in [("ctrl-alt-2", "²"), ("ctrl-alt-t", "ŧ")] {
            let mut k = Keystroke::parse(key).unwrap();
            assert!(command_for(&k).is_some(), "{key}");
            k.key_char = Some(ch.into());
            assert_eq!(command_for(&k), None, "{key} typing {ch}");
        }
    }

    #[test]
    fn trading_keys_never_repeat() {
        for tc in [
            TradeCmd::BuyMarket,
            TradeCmd::SellMarket,
            TradeCmd::JoinBid,
            TradeCmd::JoinAsk,
            TradeCmd::Reverse,
            TradeCmd::BuyAsk,
            TradeCmd::SellBid,
            TradeCmd::ToggleHotkeys,
            TradeCmd::ToggleArm,
            TradeCmd::Flatten,
            TradeCmd::CancelAll,
            TradeCmd::Breakeven,
        ] {
            assert!(!repeats(Command::Trade(tc)), "{tc:?}");
        }
        assert!(repeats(Command::Menu), "navigation may repeat");
    }

    fn cmd(s: &str) -> Option<Command> {
        // `parse` wants a `'static` str; leaking a few test strings is fine.
        command_for(&Keystroke::parse(Box::leak(s.to_owned().into_boxed_str())).unwrap())
    }

    fn ctrl() -> &'static str {
        if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        }
    }

    /// Ctrl+= and Ctrl++ zoom in, Ctrl+− out, Ctrl+0 resets (Cmd on macOS); bare keys stay
    /// with the panes; all three are in the palette.
    #[test]
    fn zoom_keys() {
        let c = |rest: &str| cmd(&format!("{}-{rest}", ctrl()));
        assert_eq!(c("="), Some(Command::ZoomIn));
        assert_eq!(c("shift-="), Some(Command::ZoomIn));
        assert_eq!(c("+"), Some(Command::ZoomIn));
        assert_eq!(c("shift-+"), Some(Command::ZoomIn));
        assert_eq!(c("-"), Some(Command::ZoomOut));
        assert_eq!(c("0"), Some(Command::ZoomReset));
        for k in ["=", "-", "0"] {
            assert_ne!(cmd(k), Some(Command::ZoomIn), "{k}");
            assert_ne!(cmd(k), Some(Command::ZoomOut), "{k}");
            assert_ne!(cmd(k), Some(Command::ZoomReset), "{k}");
        }
        let p = palette();
        for z in [Command::ZoomIn, Command::ZoomOut, Command::ZoomReset] {
            assert!(p.contains(&z), "{z:?}");
        }
        assert_eq!(label(Command::ZoomReset), "Reset zoom");
    }

    #[test]
    fn bindings() {
        let c = |rest: &str| cmd(&format!("{}-{rest}", ctrl()));
        assert_eq!(cmd("alt-left"), Some(Command::Focus(Direction::Left)));
        assert_eq!(cmd("alt-down"), Some(Command::Focus(Direction::Down)));
        assert_eq!(c("shift-right"), Some(Command::Split(Axis::Row)));
        assert_eq!(c("shift-down"), Some(Command::Split(Axis::Column)));
        assert_eq!(c("w"), Some(Command::ClosePane));
        assert_eq!(c("shift-w"), Some(Command::CloseTab));
        assert_eq!(c("f4"), Some(Command::CloseTab));
        assert_eq!(c("t"), Some(Command::NewTab));
        assert_eq!(c("shift-t"), Some(Command::ReopenTab));
        assert_eq!(c("tab"), Some(Command::NextTab));
        assert_eq!(c("shift-tab"), Some(Command::PrevTab));
        assert_eq!(c("1"), Some(Command::SelectTab(0)));
        assert_eq!(c("9"), Some(Command::SelectTab(8)));
        assert_eq!(c("shift-n"), Some(Command::TabToNewWindow));
        assert_eq!(c("l"), Some(Command::CycleLink));
        assert_eq!(c("shift-x"), Some(Command::ToggleCrosshairSync));
        assert_eq!(c("shift-g"), Some(Command::ToggleMagnet));
        // Drawings: Alt+letters that aren't the volume profile's (V, R, B, G).
        assert_eq!(
            cmd("alt-h"),
            Some(Command::Draw(DrawCmd::Tool(Tool::HLine)))
        );
        assert_eq!(
            cmd("alt-shift-h"),
            Some(Command::Draw(DrawCmd::Tool(Tool::HRay)))
        );
        assert_eq!(
            cmd("alt-t"),
            Some(Command::Draw(DrawCmd::Tool(Tool::Trend)))
        );
        assert_eq!(
            cmd("alt-shift-t"),
            Some(Command::Draw(DrawCmd::Tool(Tool::Ray)))
        );
        assert_eq!(cmd("alt-z"), Some(Command::Draw(DrawCmd::Tool(Tool::Rect))));
        assert_eq!(
            cmd("alt-l"),
            Some(Command::Draw(DrawCmd::Tool(Tool::VLine)))
        );
        assert_eq!(cmd("alt-n"), Some(Command::Draw(DrawCmd::Tool(Tool::Note))));
        assert_eq!(cmd("alt-s"), Some(Command::Draw(DrawCmd::Style)));
        assert_eq!(
            cmd("alt-u"),
            Some(Command::Draw(DrawCmd::Tool(Tool::Measure)))
        );
        for profile_key in ["v", "r", "b", "g"] {
            assert_eq!(
                cmd(&format!("alt-{profile_key}")),
                None,
                "Alt+{profile_key}"
            );
        }
        assert_eq!(c("z"), Some(Command::Draw(DrawCmd::Undo)));
        assert_eq!(c("y"), Some(Command::Draw(DrawCmd::Redo)));
        assert_eq!(c("shift-z"), Some(Command::Draw(DrawCmd::Redo)));
        // While typing (a note, the symbol picker): plain and Alt keys are the text's.
        let k = |s: &str| Keystroke::parse(Box::leak(s.to_owned().into_boxed_str())).unwrap();
        let alt_h = Command::Draw(DrawCmd::Tool(Tool::HLine));
        assert!(!runs_while_typing(&k("alt-h"), alt_h));
        assert!(!runs_while_typing(
            &k(&format!("{}-z", ctrl())),
            Command::Draw(DrawCmd::Undo)
        ));
        assert!(runs_while_typing(
            &k(&format!("{}-shift-p", ctrl())),
            Command::Palette
        ));
        // Other workspace keys are unchanged while typing.
        assert!(runs_while_typing(
            &k("alt-left"),
            Command::Focus(Direction::Left)
        ));
        assert!(runs_while_typing(&k("f1"), Command::ShowKeys));
        assert_eq!(cmd("shift-f10"), Some(Command::Menu));
        assert_eq!(c("shift-s"), Some(Command::SaveAs));
        assert_eq!(c("o"), Some(Command::Open));
        assert_eq!(c("shift-j"), Some(Command::OpenPositions));
        assert_eq!(c("shift-d"), Some(Command::Toggle(PaneKind::Dom)));
        assert_eq!(c("shift-e"), Some(Command::Toggle(PaneKind::Tape)));
        assert_eq!(c("shift-c"), Some(Command::ChartOnly));
        assert_eq!(c("shift-h"), Some(Command::ShowHidden));
        assert_eq!(c("shift-o"), Some(Command::PopOut));
        assert_eq!(c("shift-b"), Some(Command::DockBesideChart));
        assert_eq!(c("shift-p"), Some(Command::Palette));
        assert_eq!(c("shift-l"), Some(Command::ToggleStudyLabels));
        assert_eq!(c("shift-i"), Some(Command::ToggleStudies));
        assert_eq!(c("shift-m"), Some(Command::CleanChart));
        assert_eq!(cmd("f1"), Some(Command::ShowKeys));
        let t = |k: &str| Some(Command::Trade(k_trade(k)));
        assert_eq!(c("shift-f"), t("flatten"));
        assert_eq!(c("shift-a"), t("cancel"));
        assert_eq!(c("shift-space"), t("arm"));
        assert_eq!(c("shift-u"), t("account"));
        assert_eq!(c("alt-up"), t("up"));
        assert_eq!(c("alt-down"), t("down"));
        assert_eq!(c("shift-k"), t("bracket"));
        // No trading command fires from a bare key, Shift alone or Alt alone.
        for k in [
            "f", "a", "space", "shift-f", "shift-a", "alt-up", "alt-down", "up",
        ] {
            assert!(!matches!(cmd(k), Some(Command::Trade(_))), "{k}");
        }
        // Neighbours that must stay as they are: Ctrl+L links, Ctrl+I (indicators menu),
        // Ctrl+K (picker) and Alt+V (profile) belong to the chart, plain C to the DOM.
        assert_eq!(c("l"), Some(Command::CycleLink));
        for k in ["alt-v", "c", "i", "shift-f1"] {
            assert_eq!(cmd(k), None, "{k}");
        }
        // Ctrl+I opens the chart's studies menu (a command: also the palette and the menu).
        assert_eq!(c("i"), Some(Command::Studies));
        assert_eq!(c("k"), None);
    }

    fn k_trade(k: &str) -> TradeCmd {
        match k {
            "flatten" => TradeCmd::Flatten,
            "cancel" => TradeCmd::CancelAll,
            "arm" => TradeCmd::ToggleArm,
            "account" => TradeCmd::NextAccount,
            "up" => TradeCmd::QtyUp,
            "down" => TradeCmd::QtyDown,
            _ => TradeCmd::NextBracket,
        }
    }

    /// In the palette; no key of its own (pane letter keys stay unbound).
    #[test]
    fn opening_a_trades_chart_is_a_palette_command_without_a_key() {
        let p = palette();
        assert_eq!(
            filter("open the selected trade's snapshot", &p),
            vec![Command::JournalOpenChart]
        );
        assert!(!BINDINGS.iter().any(|b| b.cmd == Command::JournalOpenChart));
    }

    #[test]
    fn pnl_display_has_a_ctrl_alt_key_and_palette_entries() {
        assert_eq!(cmd(&format!("{}-alt-p", ctrl())), Some(Command::CyclePnl));
        let p = palette();
        assert!(p.contains(&Command::CyclePnl));
        for m in crate::pnl::PnlDisplay::ALL {
            assert!(p.contains(&Command::SetPnl(m)), "{m:?}");
        }
        assert!(
            filter("p&l display off", &p).contains(&Command::SetPnl(crate::pnl::PnlDisplay::Off))
        );
    }

    #[test]
    fn header_order_buttons_have_ctrl_alt_keys() {
        let ca = |k: &str| cmd(&format!("{}-alt-{k}", ctrl()));
        assert_eq!(ca("b"), Some(Command::Trade(TradeCmd::BuyMarket)));
        assert_eq!(ca("s"), Some(Command::Trade(TradeCmd::SellMarket)));
        assert_eq!(ca("j"), Some(Command::Trade(TradeCmd::JoinBid)));
        assert_eq!(ca("k"), Some(Command::Trade(TradeCmd::JoinAsk)));
        assert_eq!(ca("r"), Some(Command::Trade(TradeCmd::Reverse)));
        // Their neighbours stay what they were: Ctrl+Shift+B docks, Ctrl+Shift+S saves.
        assert_eq!(
            cmd(&format!("{}-shift-b", ctrl())),
            Some(Command::DockBesideChart)
        );
        assert_eq!(cmd(&format!("{}-shift-s", ctrl())), Some(Command::SaveAs));
        // No order fires from a bare letter, Shift or Alt alone.
        for k in [
            "b", "s", "r", "j", "k", "shift-b", "alt-b", "alt-s", "alt-r",
        ] {
            assert!(!matches!(cmd(k), Some(Command::Trade(_))), "{k}");
        }
    }

    #[test]
    fn every_trading_command_has_a_key_and_a_palette_entry() {
        let p = palette();
        for t in TradeCmd::ALL {
            assert!(!hint(Command::Trade(t)).is_empty(), "{t:?} has no key");
            assert!(p.contains(&Command::Trade(t)), "{t:?} not in the palette");
        }
        // Flatten first; the strategies' stops (which flatten) are the only other matches.
        assert_eq!(
            filter("flatten", &p),
            vec![
                Command::Trade(TradeCmd::Flatten),
                Command::Strategy(StrategyCmd::StopAll),
                Command::Strategy(StrategyCmd::Stop)
            ]
        );
        // The rest are in the palette, without keys of their own.
        for t in TradeCmd::PALETTE_ONLY {
            assert!(p.contains(&Command::Trade(t)), "{t:?} not in the palette");
            assert!(hint(Command::Trade(t)).is_empty(), "{t:?} has a key");
        }
        // Stop all strategies has a key (from any pane); starting one never does.
        assert!(!hint(Command::Strategy(StrategyCmd::StopAll)).is_empty());
        assert_eq!(
            cmd(&format!("{}-shift-r", ctrl())),
            Some(Command::Strategy(StrategyCmd::StopAll))
        );
        assert!(hint(Command::Strategy(StrategyCmd::Start)).is_empty());
        assert!(!repeats(Command::Strategy(StrategyCmd::StopAll)));
        for c in StrategyCmd::ALL {
            assert!(
                p.contains(&Command::Strategy(c)),
                "{c:?} not in the palette"
            );
        }
        // US English names: enable, disable, toggle (never "arm").
        for word in ["enable", "disable", "toggle"] {
            assert!(!filter(&format!("trade {word}"), &p).is_empty(), "{word}");
        }
        assert!(
            filter("arm", &p)
                .iter()
                .all(|c| !label(*c).to_lowercase().contains(" arm"))
        );
    }

    #[test]
    fn no_key_is_bound_twice() {
        for (i, a) in BINDINGS.iter().enumerate() {
            for b in &BINDINGS[i + 1..] {
                assert!(
                    !(a.ctrl == b.ctrl && a.shift == b.shift && a.alt == b.alt && a.key == b.key),
                    "{:?} and {:?} share a key",
                    a.cmd,
                    b.cmd
                );
            }
        }
    }

    #[test]
    fn pane_keys_stay_with_the_panes() {
        // Letters open the picker, C/G drive the DOM, Home resets the chart, Ctrl+K opens the
        // picker, Ctrl+D and Alt+M work inside it, Alt+V/R/B/G drive the chart's volume
        // profile: none of them is a workspace command.
        for k in [
            "a", "c", "g", "shift-g", "home", "escape", "enter", "alt-m", "0",
            // Chart volume profile: Alt+V/R/B/G.
            "alt-v", "alt-r", "alt-b", "alt-g",
        ] {
            assert_eq!(cmd(k), None, "{k}");
        }
        for k in ["k", "d"] {
            assert_eq!(cmd(&format!("{}-{k}", ctrl())), None, "{k}");
        }
        // Ctrl+I is the Studies command (it acts on the focused chart; decision 0018).
        assert_eq!(cmd(&format!("{}-i", ctrl())), Some(Command::Studies));
        assert_eq!(cmd("ctrl-alt-left"), None);
    }

    /// Keycaps: one key per cap, modifiers in the platform's order, the same keys as the text.
    #[test]
    fn keys_are_one_per_cap_in_the_platforms_order() {
        let k = |c, mac| keys_on(c, mac);
        assert_eq!(k(Command::Split(Axis::Row), false), ["Ctrl", "Shift", "→"]);
        assert_eq!(k(Command::Split(Axis::Row), true), ["⇧", "⌘", "→"]);
        assert_eq!(k(Command::Menu, false), ["Shift", "F10"]);
        assert_eq!(k(Command::SelectTab(2), false), ["Ctrl", "3"]);
        assert_eq!(k(Command::SelectTab(2), true), ["⌘", "3"]);
        assert_eq!(k(Command::Focus(Direction::Up), false), ["Alt", "arrows"]);
        assert_eq!(
            k(Command::Strategy(StrategyCmd::Start), false),
            Vec::<String>::new()
        );
        // The text form is the same keys joined.
        assert_eq!(hint_on(Command::Split(Axis::Row), false), "Ctrl+Shift+→");
        assert_eq!(hint_on(Command::Split(Axis::Row), true), "⇧⌘→");
        assert_eq!(hint_on(Command::Focus(Direction::Up), true), "⌥arrows");
        for b in BINDINGS {
            assert!(!keys_on(b.cmd, false).is_empty(), "{:?}", b.cmd);
        }
    }

    #[test]
    fn hints_come_from_the_bindings() {
        if cfg!(target_os = "macos") {
            assert_eq!(hint(Command::CloseTab), "⌘F4");
            assert_eq!(hint(Command::Split(Axis::Row)), "⇧⌘→");
        } else {
            assert_eq!(hint(Command::CloseTab), "Ctrl+F4");
            assert_eq!(hint(Command::Split(Axis::Row)), "Ctrl+Shift+→");
            assert_eq!(hint(Command::ReopenTab), "Ctrl+Shift+T");
            assert_eq!(hint(Command::Menu), "Shift+F10");
        }
        assert_eq!(hint(Command::SetLink(Some(1))), "");
    }

    #[test]
    fn every_bound_command_is_in_the_palette() {
        // Discoverability: anything with a hotkey can also be found by name.
        let p = palette();
        for b in BINDINGS {
            // The palette itself, and docking (a popped-out pane window's command), aside.
            let in_palette =
                p.contains(&b.cmd) || matches!(b.cmd, Command::DockBesideChart | Command::Palette);
            assert!(in_palette, "{:?} has a key but isn't in the palette", b.cmd);
        }
    }

    #[test]
    fn palette_filters_by_words() {
        let p = palette();
        assert_eq!(filter("reopen", &p), vec![Command::ReopenTab]);
        assert_eq!(
            filter("hide/show dom", &p),
            vec![Command::Toggle(PaneKind::Dom)]
        );
        assert!(filter("layout", &p).len() >= 5);
        assert_eq!(filter("", &p).len(), p.len());
        assert!(filter("zzz", &p).is_empty());
        assert_eq!(
            filter("reset columns", &p),
            vec![Command::Columns(ColumnCmd::Reset)]
        );
        assert_eq!(
            filter("dom column vol", &p),
            vec![Command::Columns(ColumnCmd::Toggle(Column::Dom(
                DomCol::Volume
            )))]
        );
        assert_eq!(filter("performance", &p), vec![Command::TogglePerfStats]);
        // Timeframes: one entry each, "Timeframe: 1m" … "Timeframe: 1W"; no keys of their own
        // (digits and letters typed on a chart are the chart's).
        assert_eq!(
            filter("timeframe 15m", &p),
            vec![Command::Timeframe(Timeframe::Minutes(15))]
        );
        assert_eq!(
            filter("timeframe 1w", &p),
            vec![Command::Timeframe(Timeframe::Week)]
        );
        assert_eq!(
            p.iter()
                .filter(|c| matches!(c, Command::Timeframe(_)))
                .count(),
            Timeframe::ALL.len()
        );
        assert!(
            BINDINGS
                .iter()
                .all(|b| !matches!(b.cmd, Command::Timeframe(_) | Command::TimeframeMenu))
        );
        // A palette/menu command: no key of its own.
        assert!(BINDINGS.iter().all(|b| b.cmd != Command::TogglePerfStats));
        assert_eq!(filter("latency", &p), vec![Command::ToggleLatencyHud]);
        assert!(BINDINGS.iter().all(|b| b.cmd != Command::ToggleLatencyHud));
        // The DOM trail: palette and pane menu only, never a key (pane keys are reserved).
        assert_eq!(
            filter("price trail", &p),
            vec![Command::DomTrail, Command::DomTrailHalfLife]
        );
        assert!(
            BINDINGS
                .iter()
                .all(|b| !matches!(b.cmd, Command::DomTrail | Command::DomTrailHalfLife))
        );
    }
}
