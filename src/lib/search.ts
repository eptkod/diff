// How well one searched word fits a title, best first.
const EXACT_WORD = 4;
const WORD_START = 3;
const INSIDE_WORD = 2;
const CLOSE_SPELLING = 1;
const IGNORING_SPACES = 1;
const WHOLE_PHRASE_BONUS = 5;
const MATCHED_WORD_WEIGHT = 100;
const MIN_LENGTH_FOR_PARTIAL_MATCH = 3;

const MIN_LENGTH_FOR_ONE_TYPO = 4;
const MIN_LENGTH_FOR_TWO_TYPOS = 6;
const MIN_LENGTH_FOR_PARTIAL_TYPO = 5;

/** Lowercases, drops accents and reduces punctuation to single spaces. */
const normalize = (text: string) =>
  text
    .normalize("NFKD")
    .replace(/\p{M}/gu, "")
    .toLowerCase()
    .replace(/[^\p{L}\p{N}]+/gu, " ")
    .trim();

/** Edits needed to turn one word into the other, counting a swap of neighbours as one. */
function editDistance(a: string, b: string) {
  const rows = Array.from({ length: a.length + 1 }, (_, i) => [i, ...new Array<number>(b.length).fill(0)]);
  for (let j = 1; j <= b.length; j++) rows[0][j] = j;
  for (let i = 1; i <= a.length; i++) {
    for (let j = 1; j <= b.length; j++) {
      const replaced = rows[i - 1][j - 1] + (a[i - 1] === b[j - 1] ? 0 : 1);
      rows[i][j] = Math.min(rows[i - 1][j] + 1, rows[i][j - 1] + 1, replaced);
      if (i > 1 && j > 1 && a[i - 1] === b[j - 2] && a[i - 2] === b[j - 1]) {
        rows[i][j] = Math.min(rows[i][j], rows[i - 2][j - 2] + 1);
      }
    }
  }
  return rows[a.length][b.length];
}

function isCloseSpelling(query: string, word: string) {
  // Numbers are exact: "clip 12" shouldn't find "clip 13".
  if (query.length < MIN_LENGTH_FOR_ONE_TYPO || /\p{N}/u.test(query) || query[0] !== word[0]) return false;
  const allowed = query.length >= MIN_LENGTH_FOR_TWO_TYPOS ? 2 : 1;
  if (editDistance(query, word) <= allowed) return true;
  // Covers a typo in a word that's only partly typed so far.
  return query.length >= MIN_LENGTH_FOR_PARTIAL_TYPO && editDistance(query, word.slice(0, query.length)) <= 1;
}

function wordScore(query: string, titleWords: string[]) {
  let best = 0;
  for (const word of titleWords) {
    if (word === query) return EXACT_WORD;
    const score = word.startsWith(query)
      ? WORD_START
      : word.includes(query)
        ? INSIDE_WORD
        : isCloseSpelling(query, word)
          ? CLOSE_SPELLING
          : 0;
    best = Math.max(best, score);
  }
  return best;
}

/**
 * Scores a normalized title against a normalized query; 0 means no match.
 * Each matched word outweighs how well the words match, so a title with only
 * some of the words always ranks below one with all of them.
 */
function matchScore(title: string, query: string) {
  const titleWords = title.split(" ");
  const queryWords = query.split(" ");
  const matchedWords = queryWords.filter((word) => wordScore(word, titleWords) > 0);
  const fit = matchedWords.reduce((sum, word) => sum + wordScore(word, titleWords), 0);
  if (matchedWords.length === queryWords.length) {
    return queryWords.length * MATCHED_WORD_WEIGHT + fit + (title.includes(query) ? WHOLE_PHRASE_BONUS : 0);
  }
  if (title.replaceAll(" ", "").includes(query.replaceAll(" ", ""))) {
    return queryWords.length * MATCHED_WORD_WEIGHT + IGNORING_SPACES;
  }
  // "it" alone would otherwise bring up every title containing those letters.
  const worthShowing = matchedWords.some((word) => word.length >= MIN_LENGTH_FOR_PARTIAL_MATCH);
  return worthShowing ? matchedWords.length * MATCHED_WORD_WEIGHT + fit : 0;
}

/** The items whose text matches the query, closest first; equal matches keep their order. */
export function searchBy<T>(items: T[], query: string, text: (item: T) => string): T[] {
  const normalized = normalize(query);
  if (!normalized) return items;
  return items
    .map((item) => ({ item, score: matchScore(normalize(text(item)), normalized) }))
    .filter(({ score }) => score > 0)
    .sort((a, b) => b.score - a.score)
    .map(({ item }) => item);
}
