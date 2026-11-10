import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type ListedSource =
  | { kind: "display"; id: number }
  | { kind: "window"; id: number }
  | { kind: "region"; display_id: number; x: number; y: number; width: number; height: number };

export type Source = ListedSource | { kind: "activeWindow" };

export type Quality = "low" | "medium" | "high";

export type VoiceModel = "light" | "accurate";

export interface Settings {
  source: Source | null;
  clipSeconds: number;
  hotkey: string;
  voiceEnabled: boolean;
  voiceModel: VoiceModel;
  includeMic: boolean;
  quality: Quality;
  saveFolder: string;
  startAtLogin: boolean;
  setupComplete: boolean;
}

export type Status =
  | { state: "stopped" | "recording" | "paused" | "needsPermission" }
  | { state: "error"; message: string };

export interface AppState {
  settings: Settings;
  status: Status;
  hasPermission: boolean;
  voiceIssue: string | null;
  following: string | null;
}

export interface SourceInfo {
  source: ListedSource;
  name: string;
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface Clip {
  path: string;
  name: string;
  sizeBytes: number;
  durationSeconds: number;
  modifiedMs: number;
  starred: boolean;
  thumbnail: string | null;
}

export const CLIP_LENGTHS = [15, 30, 60, 120, 300];

export const QUALITIES: { id: Quality; label: string; detail: string; bitrate: number }[] = [
  { id: "low", label: "Low", detail: "720p, 30 fps", bitrate: 5_000_000 },
  { id: "medium", label: "Medium", detail: "1080p, 30 fps", bitrate: 10_000_000 },
  { id: "high", label: "High", detail: "1080p, 60 fps", bitrate: 15_000_000 },
];

export const VOICE_MODELS: { id: VoiceModel; label: string; detail: string }[] = [
  {
    id: "accurate",
    label: "Accurate",
    detail: "Hears quiet and misheard commands much more reliably. Uses about 200 MB more memory, and a little processing after you speak.",
  },
  {
    id: "light",
    label: "Light",
    detail: "Uses almost no memory or processing, but often needs a clear voice close to the microphone.",
  },
];

export const api = {
  getState: () => invoke<AppState>("get_state"),
  saveSettings: (settings: Settings) => invoke<AppState>("save_settings", { settings }),
  finishSetup: () => invoke<void>("finish_setup"),
  retryCapture: () => invoke<AppState>("retry_capture"),
  requestScreenPermission: () => invoke<boolean>("request_screen_permission"),
  openPermissionSettings: () => invoke<void>("open_permission_settings"),
  openMicrophoneSettings: () => invoke<void>("open_microphone_settings"),
  listSources: () => invoke<SourceInfo[]>("list_sources"),
  chooseFolder: () => invoke<string | null>("choose_folder"),
  openRegionPicker: (displayId: number) => invoke<void>("open_region_picker", { displayId }),
  setRegion: (displayId: number, x: number, y: number, width: number, height: number) =>
    invoke<AppState>("set_region", { displayId, x, y, width, height }),
  openSettings: () => invoke<void>("open_settings_window"),
  clipNow: () => invoke<void>("clip_now"),
  listClips: () => invoke<Clip[]>("list_clips"),
  deleteClip: (path: string) => invoke<void>("delete_clip", { path }),
  deleteClips: (paths: string[]) => invoke<void>("delete_clips", { paths }),
  setClipStarred: (path: string, starred: boolean) =>
    invoke<void>("set_clip_starred", { path, starred }),
  setClipsStarred: (paths: string[], starred: boolean) =>
    invoke<void>("set_clips_starred", { paths, starred }),
  renameClip: (path: string, name: string) => invoke<string>("rename_clip", { path, name }),
  trimClip: (path: string, start: number, end: number) =>
    invoke<string>("trim_clip", { path, start, end }),
  revealClip: (path: string) => invoke<void>("reveal_clip", { path }),
  saveThumbnail: (path: string, modifiedMs: number, jpeg: number[]) =>
    invoke<string>("save_thumbnail", { path, modifiedMs, jpeg }),
};

export const onClipsChanged = (run: () => void) => listen("clips-changed", run);
export const onSettingsChanged = (run: () => void) => listen("settings-changed", run);
export const mediaUrl = (path: string) => convertFileSrc(path);
export const isMac = () => navigator.userAgent.includes("Mac");
export const revealLabel = () => (isMac() ? "Show in Finder" : "Show in File Explorer");

/** Turns the error thrown by a failed command into a sentence for the UI. */
export const errorText = (error: unknown) => (typeof error === "string" ? error : String(error));

export function formatLength(seconds: number) {
  return seconds < 60 ? `${seconds} seconds` : seconds === 60 ? "1 minute" : `${seconds / 60} minutes`;
}

export function formatDuration(totalSeconds: number) {
  const rounded = Math.round(totalSeconds);
  return `${Math.floor(rounded / 60)}:${String(rounded % 60).padStart(2, "0")}`;
}

export function formatSize(bytes: number) {
  return bytes < 1024 * 1024 * 1024
    ? `${Math.max(1, Math.round(bytes / (1024 * 1024)))} MB`
    : `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

export function formatDate(ms: number) {
  return new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

const AUTO_NAME = /^diff-\d{4}-\d{2}-\d{2}_\d{2}-\d{2}-\d{2}(-\d+)?$/;

/** Auto-generated names are noise, so those clips are titled by their date. */
export const clipTitle = (clip: Clip) =>
  AUTO_NAME.test(clip.name) ? formatDate(clip.modifiedMs) : clip.name;

const KEY_LABELS: Record<string, string> = {
  Control: "Ctrl",
  Alt: "Alt",
  Shift: "Shift",
  Super: "Win",
};
const MAC_KEY_LABELS: Record<string, string> = {
  Control: "⌃",
  Alt: "⌥",
  Shift: "⇧",
  Super: "⌘",
};

/** Displays a stored shortcut such as `Alt+Shift+KeyC` for the current OS. */
export function formatHotkey(hotkey: string) {
  const labels = isMac() ? MAC_KEY_LABELS : KEY_LABELS;
  const parts = hotkey.split("+").map((part) => labels[part] ?? part.replace(/^(Key|Digit)/, ""));
  return parts.join(isMac() ? "" : " + ");
}

/** Converts a key press into a shortcut, or null if it isn't usable as one. */
export function hotkeyFromEvent(event: KeyboardEvent): string | null {
  if (["Control", "Shift", "Alt", "Meta"].includes(event.key)) return null;
  const modifiers = [
    event.ctrlKey && "Control",
    event.altKey && "Alt",
    event.shiftKey && "Shift",
    event.metaKey && "Super",
  ].filter(Boolean) as string[];
  // A bare letter would fire every time you typed it.
  if (modifiers.length === 0 && !/^F\d{1,2}$/.test(event.code)) return null;
  return [...modifiers, event.code].join("+");
}
