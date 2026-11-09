import type { Clip } from "$lib/api";

export type DateFilter = "any" | "today" | "week" | "month";
export type LengthFilter = "any" | "short" | "medium" | "long";

export interface ClipFilters {
  query: string;
  starred: boolean;
  date: DateFilter;
  length: LengthFilter;
}

export const NO_FILTERS: ClipFilters = { query: "", starred: false, date: "any", length: "any" };

export const DATE_OPTIONS: { id: DateFilter; label: string }[] = [
  { id: "any", label: "Any date" },
  { id: "today", label: "Today" },
  { id: "week", label: "Last 7 days" },
  { id: "month", label: "Last 30 days" },
];

export const LENGTH_OPTIONS: { id: LengthFilter; label: string }[] = [
  { id: "any", label: "Any length" },
  { id: "short", label: "Under 45 seconds" },
  { id: "medium", label: "45 seconds to 3 minutes" },
  { id: "long", label: "Over 3 minutes" },
];

// Saved clips run slightly over or under their set length, so these limits sit
// between the lengths in CLIP_LENGTHS rather than on them.
const SHORT_LIMIT_SECONDS = 45;
const LONG_LIMIT_SECONDS = 180;

const DATE_RANGE_DAYS: Record<Exclude<DateFilter, "any">, number> = { today: 1, week: 7, month: 30 };

/** The start of the local day that begins the range, counting today as one day. */
function rangeStart(date: Exclude<DateFilter, "any">, now: number) {
  const start = new Date(now);
  start.setHours(0, 0, 0, 0);
  start.setDate(start.getDate() - (DATE_RANGE_DAYS[date] - 1));
  return start.getTime();
}

function matchesLength(seconds: number, length: LengthFilter) {
  switch (length) {
    case "short":
      return seconds < SHORT_LIMIT_SECONDS;
    case "medium":
      return seconds >= SHORT_LIMIT_SECONDS && seconds <= LONG_LIMIT_SECONDS;
    case "long":
      return seconds > LONG_LIMIT_SECONDS;
    default:
      return true;
  }
}

/** Applies every filter except the name search, which also ranks; see `searchBy`. */
export function clipMatches(clip: Clip, filters: ClipFilters, now: number) {
  return (
    (!filters.starred || clip.starred) &&
    (filters.date === "any" || clip.modifiedMs >= rangeStart(filters.date, now)) &&
    matchesLength(clip.durationSeconds, filters.length)
  );
}

export const isFiltering = (filters: ClipFilters) =>
  filters.query !== "" || filters.starred || filters.date !== "any" || filters.length !== "any";
