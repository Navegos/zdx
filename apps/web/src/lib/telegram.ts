/**
 * Telegram WebApp bridge.
 *
 * Deliberately talks to `window.Telegram.WebApp` directly rather than through a
 * wrapper SDK: the surface we need is small, and the global is guaranteed
 * present because telegram-web-app.js is loaded synchronously in index.html.
 */

type HapticStyle = "light" | "medium" | "heavy" | "rigid" | "soft";

interface TelegramWebApp {
  initData: string;
  initDataUnsafe?: { start_param?: string; user?: { id: number } };
  colorScheme?: "light" | "dark";
  themeParams?: Record<string, string>;
  isExpanded?: boolean;
  viewportStableHeight?: number;
  safeAreaInset?: { top: number; bottom: number; left: number; right: number };
  contentSafeAreaInset?: { top: number; bottom: number; left: number; right: number };
  version?: string;
  ready(): void;
  expand(): void;
  close(): void;
  onEvent(event: string, handler: () => void): void;
  offEvent(event: string, handler: () => void): void;
  isVersionAtLeast?(version: string): boolean;
  openTelegramLink?(url: string): void;
  requestFullscreen?(): void;
  enableClosingConfirmation?(): void;
  showConfirm?(message: string, callback: (ok: boolean) => void): void;
  disableClosingConfirmation?(): void;
  disableVerticalSwipes?(): void;
  setHeaderColor?(color: string): void;
  setBackgroundColor?(color: string): void;
  BackButton?: {
    isVisible: boolean;
    show(): void;
    hide(): void;
    onClick(cb: () => void): void;
    offClick(cb: () => void): void;
  };
  HapticFeedback?: {
    impactOccurred?(style: HapticStyle): void;
    selectionChanged?(): void;
    notificationOccurred?(type: "error" | "success" | "warning"): void;
  };
}

declare global {
  interface Window {
    Telegram?: { WebApp?: TelegramWebApp };
  }
}

export const tg: TelegramWebApp | undefined =
  typeof window !== "undefined" ? window.Telegram?.WebApp : undefined;

/** True when running inside a real Telegram client (initData is signed). */
export const inTelegram = Boolean(tg?.initData);

export function haptic(style: HapticStyle = "light"): void {
  tg?.HapticFeedback?.impactOccurred?.(style);
}

export function selectionChanged(): void {
  tg?.HapticFeedback?.selectionChanged?.();
}

export function notify(type: "error" | "success" | "warning"): void {
  tg?.HapticFeedback?.notificationOccurred?.(type);
}

/** Telegram user id from initData, or `"dev"` outside Telegram. Scopes local drafts. */
export function telegramUserId(): string {
  return String(tg?.initDataUnsafe?.user?.id ?? "dev");
}

/** Ask before Telegram closes the Mini App (unsent drafts). Saved drafts survive either way. */
export function setClosingConfirmation(on: boolean): void {
  if (on) tg?.enableClosingConfirmation?.();
  else tg?.disableClosingConfirmation?.();
}

/** A yes/no question: Telegram's native popup inside Telegram, `confirm` elsewhere. */
export function confirmAction(message: string): Promise<boolean> {
  const show = tg?.showConfirm;
  if (show && tg?.isVersionAtLeast?.("6.2")) {
    return new Promise((resolve) => show.call(tg, message, (ok) => resolve(Boolean(ok))));
  }
  return Promise.resolve(window.confirm(message));
}

/** Telegram's `startapp=` payload, used to deep-link straight to a thread. */
export function startParam(): string {
  return String(tg?.initDataUnsafe?.start_param ?? "").trim();
}

/**
 * Jumps to a `t.me` link inside the Telegram client.
 *
 * Since Bot API 7.0 the Mini App is *not* closed, so the user can come back.
 * Outside Telegram (browser dev) this falls back to a normal new-tab open.
 */
export function openTelegramLink(url: string): void {
  if (tg?.openTelegramLink) {
    tg.openTelegramLink(url);
    return;
  }
  window.open(url, "_blank", "noopener,noreferrer");
}

function applyScheme(): void {
  const scheme = tg?.colorScheme;
  if (scheme) document.documentElement.dataset.scheme = scheme;
}

function applySafeArea(): void {
  const root = document.documentElement;
  const top = tg?.contentSafeAreaInset?.top ?? tg?.safeAreaInset?.top ?? 0;
  const bottom = tg?.safeAreaInset?.bottom ?? 0;
  root.style.setProperty("--tg-safe-top", `${top}px`);
  root.style.setProperty("--tg-safe-bottom", `${bottom}px`);
}

/** Call once at startup, before mounting. */
export function initTelegram(): void {
  if (!tg) {
    // Browser dev: follow the OS preference.
    document.documentElement.dataset.scheme = window.matchMedia?.(
      "(prefers-color-scheme: dark)",
    ).matches
      ? "dark"
      : "light";
    return;
  }

  tg.ready();
  tg.expand();
  tg.disableVerticalSwipes?.();
  tg.setHeaderColor?.("secondary_bg_color");
  tg.setBackgroundColor?.("bg_color");

  applyScheme();
  applySafeArea();

  tg.onEvent("themeChanged", applyScheme);
  tg.onEvent("safeAreaChanged", applySafeArea);
  tg.onEvent("contentSafeAreaChanged", applySafeArea);
}

/** Wire the native Back button to a handler; returns a cleanup function. */
export function backButton(onBack: () => void): () => void {
  const bb = tg?.BackButton;
  if (!bb) return () => {};
  bb.onClick(onBack);
  bb.show();
  return () => {
    bb.offClick(onBack);
    bb.hide();
  };
}
