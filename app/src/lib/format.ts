/** Bytes → a compact human string (e.g. `4 GB`). */
export function humanBytes(bytes: number): string {
	if (!bytes || bytes <= 0) return '0 B';
	const units = ['B', 'KB', 'MB', 'GB', 'TB'];
	const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
	const val = bytes / Math.pow(1024, i);
	const rounded = val >= 100 || Number.isInteger(val) ? Math.round(val) : Math.round(val * 10) / 10;
	return `${rounded} ${units[i]}`;
}

/** GiB ⇄ bytes helpers for the budget sliders. */
export const GIB = 1024 * 1024 * 1024;
export const bytesToGib = (b: number) => Math.round((b / GIB) * 10) / 10;
export const gibToBytes = (g: number) => Math.round(g * GIB);

/** Truncate a long id/address to `abcd…wxyz`. */
export function shortId(id: string | null | undefined, head = 6, tail = 6): string {
	if (!id) return '—';
	if (id.length <= head + tail + 1) return id;
	return `${id.slice(0, head)}…${id.slice(-tail)}`;
}

/** Seconds → `1d 2h 3m` style uptime. */
export function uptime(secs: number): string {
	if (!secs || secs <= 0) return '0s';
	const d = Math.floor(secs / 86400);
	const h = Math.floor((secs % 86400) / 3600);
	const m = Math.floor((secs % 3600) / 60);
	const s = secs % 60;
	const parts: string[] = [];
	if (d) parts.push(`${d}d`);
	if (h) parts.push(`${h}h`);
	if (m) parts.push(`${m}m`);
	if (!d && !h) parts.push(`${s}s`);
	return parts.join(' ');
}

export function num(n: number | null | undefined): string {
	if (n == null) return '—';
	return new Intl.NumberFormat('en-US').format(n);
}
