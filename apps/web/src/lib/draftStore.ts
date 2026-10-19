/**
 * Unsent annotation drafts, kept in IndexedDB so they survive closing the
 * preview, switching tabs, closing the Mini App, and an expired Telegram
 * authorization (the only fix for that is reopening the Mini App).
 *
 * One record per (Telegram user, thread, artifact path). The record keeps the
 * SHA-256 of the version the notes were made on, so a reopened artifact whose
 * file changed is shown as stale instead of silently re-anchoring the notes,
 * and the submission id, so a retry after reopening is still recognized by the
 * server as the same submission.
 *
 * Media are stored as Blobs. When IndexedDB is not available or a write fails
 * (some private modes, storage full), records live in memory for the session
 * only and `saveDraftRecord` reports it.
 */

import type { DraftAnnotation, ImageFrame } from "./annotate";

export interface DraftRecord {
  key: string;
  /** Version the drafts were made on. */
  sha: string;
  /** Image artifacts: the canonical copy the marks are fractions of. */
  frame?: ImageFrame;
  submissionId: string;
  drafts: DraftAnnotation[];
  updatedAt: number;
}

const DB_NAME = "zdx-annotations";
const STORE = "drafts";
const memory = new Map<string, DraftRecord>();
let dbPromise: Promise<IDBDatabase | null> | null = null;

function open(): Promise<IDBDatabase | null> {
  if (dbPromise) return dbPromise;
  dbPromise = new Promise((resolve) => {
    let request: IDBOpenDBRequest;
    try {
      // Even reading `indexedDB` can throw (disabled storage, sandboxed WebViews).
      if (typeof indexedDB === "undefined") return resolve(null);
      request = indexedDB.open(DB_NAME, 1);
    } catch {
      return resolve(null);
    }
    request.onupgradeneeded = () => {
      if (!request.result.objectStoreNames.contains(STORE)) request.result.createObjectStore(STORE, { keyPath: "key" });
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => resolve(null);
    request.onblocked = () => resolve(null);
  });
  return dbPromise;
}

function run<T>(db: IDBDatabase, mode: IDBTransactionMode, body: (store: IDBObjectStore) => IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, mode);
    const request = body(tx.objectStore(STORE));
    tx.oncomplete = () => resolve(request.result);
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error);
  });
}

export function draftKey(userId: string, threadId: string, path: string): string {
  return `${userId}\n${threadId}\n${path}`;
}

export async function loadDraftRecord(key: string): Promise<DraftRecord | null> {
  const db = await open();
  if (!db) return memory.get(key) ?? null;
  try {
    return ((await run(db, "readonly", (store) => store.get(key))) as DraftRecord | undefined) ?? null;
  } catch {
    return memory.get(key) ?? null;
  }
}

/**
 * Saves the record, or deletes it when it has no drafts. `false` when the
 * device store failed: the drafts then live in memory only, until the Mini
 * App closes, and the UI must say so.
 */
export async function saveDraftRecord(record: DraftRecord): Promise<boolean> {
  // `$state` proxies cannot be stored; callers pass plain snapshots.
  const empty = record.drafts.length === 0;
  if (empty) memory.delete(record.key);
  else memory.set(record.key, record);
  const db = await open();
  if (!db) return false;
  try {
    if (empty) await run(db, "readwrite", (store) => store.delete(record.key));
    else await run(db, "readwrite", (store) => store.put(record));
    return true;
  } catch (e) {
    console.warn("saving annotation drafts failed", e);
    return false;
  }
}
