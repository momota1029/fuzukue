//! Lucide SVGs. See the license notices in assets/icons.

use gpui_kit::component::Icon;
use gpui_kit::{prelude::*, px, Hsla};

#[derive(Clone, Copy)]
pub(super) enum Symbol {
    Archive,
    Camera,
    ChartNoAxesColumn,
    Folder,
    FolderGit,
    Inbox,
    Info,
    MessageSquare,
    Pin,
    Server,
    ArrowLeftRight,
    ArrowLeft,
    ArrowRight,
    BookOpen,
    Check,
    Copy,
    FileDiff,
    ChevronDown,
    ChevronRight,
    CircleDot,
    Circle,
    Diamond,
    Ellipsis,
    FilePenLine,
    FileText,
    GitBranch,
    LayoutList,
    Menu,
    Minus,
    PanelsTopLeft,
    Pause,
    Plus,
    Search,
    Library,
    Settings,
    TriangleAlert,
    X,
    Trash,
    RotateCcw,
}

impl Symbol {
    pub(super) fn icon(self) -> Icon {
        let bytes: &[u8] = match self {
            Self::Camera => include_bytes!("../assets/icons/camera.svg"),
            Self::Archive => include_bytes!("../assets/icons/archive.svg"),
            Self::ChartNoAxesColumn => include_bytes!("../assets/icons/chart-no-axes-column.svg"),
            Self::Folder => include_bytes!("../assets/icons/folder.svg"),
            Self::FolderGit => include_bytes!("../assets/icons/folder-git.svg"),
            Self::Inbox => include_bytes!("../assets/icons/inbox.svg"),
            Self::Info => include_bytes!("../assets/icons/info.svg"),
            Self::MessageSquare => include_bytes!("../assets/icons/message-square.svg"),
            Self::Pin => include_bytes!("../assets/icons/pin.svg"),
            Self::Server => include_bytes!("../assets/icons/server.svg"),
            Self::ArrowLeftRight => include_bytes!("../assets/icons/arrow-left-right.svg"),
            Self::ArrowLeft => include_bytes!("../assets/icons/arrow-left.svg"),
            Self::ArrowRight => include_bytes!("../assets/icons/arrow-right.svg"),
            Self::BookOpen => include_bytes!("../assets/icons/book-open.svg"),
            Self::Check => include_bytes!("../assets/icons/check.svg"),
            Self::Copy => include_bytes!("../assets/icons/copy.svg"),
            Self::FileDiff => include_bytes!("../assets/icons/file-diff.svg"),
            Self::ChevronDown => include_bytes!("../assets/icons/chevron-down.svg"),
            Self::ChevronRight => include_bytes!("../assets/icons/chevron-right.svg"),
            Self::CircleDot => include_bytes!("../assets/icons/circle-dot.svg"),
            Self::Circle => include_bytes!("../assets/icons/circle.svg"),
            Self::Diamond => include_bytes!("../assets/icons/diamond.svg"),
            Self::Ellipsis => include_bytes!("../assets/icons/ellipsis.svg"),
            Self::FilePenLine => include_bytes!("../assets/icons/file-pen-line.svg"),
            Self::FileText => include_bytes!("../assets/icons/file-text.svg"),
            Self::GitBranch => include_bytes!("../assets/icons/git-branch.svg"),
            Self::LayoutList => include_bytes!("../assets/icons/layout-list.svg"),
            Self::Menu => include_bytes!("../assets/icons/menu.svg"),
            Self::Minus => include_bytes!("../assets/icons/minus.svg"),
            Self::PanelsTopLeft => include_bytes!("../assets/icons/panels-top-left.svg"),
            Self::Pause => include_bytes!("../assets/icons/pause.svg"),
            Self::Plus => include_bytes!("../assets/icons/plus.svg"),
            Self::Search => include_bytes!("../assets/icons/search.svg"),
            Self::Library => include_bytes!("../assets/icons/library.svg"),
            Self::Settings => include_bytes!("../assets/icons/settings.svg"),
            Self::TriangleAlert => include_bytes!("../assets/icons/triangle-alert.svg"),
            Self::Trash => include_bytes!("../assets/icons/trash.svg"),
            Self::RotateCcw => include_bytes!("../assets/icons/rotate-ccw.svg"),
            Self::X => include_bytes!("../assets/icons/x.svg"),
        };
        Icon::default().data(bytes)
    }

    pub(super) fn tinted(self, color: Hsla) -> Icon {
        self.icon().size(px(16.)).text_color(color)
    }
}
