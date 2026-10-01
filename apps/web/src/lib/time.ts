/** `resets in 2d 4h` / `resets in 3h 12m` / `resets in 8m`, measured against
 *  `now` so the countdown stays right while the snapshot ages. */
export function resetCountdown(resetsAt: Date, now: number): string {
  const secs = Math.floor((resetsAt.getTime() - now) / 1000);
  if (secs <= 0) return "reset due";
  const days = Math.floor(secs / 86_400);
  const hours = Math.floor((secs % 86_400) / 3_600);
  const mins = Math.floor((secs % 3_600) / 60);
  if (days > 0) return `resets in ${days}d ${hours}h`;
  if (hours > 0) return `resets in ${hours}h ${mins}m`;
  return `resets in ${Math.max(mins, 1)}m`;
}

const timeFmt = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" });
const weekdayFmt = new Intl.DateTimeFormat(undefined, { weekday: "short" });
const dateFmt = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" });

function dayStart(t: number): number {
  const d = new Date(t);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** The reset instant in local time: `today 14:30`, `tomorrow 09:00`,
 *  `Thu 14:30` within the week, `Oct 8 14:30` beyond it. */
export function resetClock(resetsAt: Date, now: number): string {
  const days = Math.round((dayStart(resetsAt.getTime()) - dayStart(now)) / 86_400_000);
  const time = timeFmt.format(resetsAt);
  if (days === 0) return `today ${time}`;
  if (days === 1) return `tomorrow ${time}`;
  if (days > 1 && days < 7) return `${weekdayFmt.format(resetsAt)} ${time}`;
  return `${dateFmt.format(resetsAt)} ${time}`;
}
