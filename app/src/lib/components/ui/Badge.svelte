<script lang="ts">
	import { cn } from '$lib/utils';
	import type { Snippet } from 'svelte';

	type Variant = 'brand' | 'ok' | 'warn' | 'muted' | 'danger';

	let {
		variant = 'muted',
		class: className = '',
		children
	}: { variant?: Variant; class?: string; children?: Snippet } = $props();

	const variants: Record<Variant, string> = {
		brand: 'border-[#ffd40040] text-brand',
		ok: 'border-emerald-400/40 text-emerald-300',
		warn: 'border-amber-400/40 text-amber-300',
		danger: 'border-red-400/40 text-red-300',
		muted: 'border-white/15 text-white/60'
	};
</script>

<span
	class={cn(
		'inline-flex items-center gap-1.5 rounded-full border px-2.5 py-0.5 text-xs font-medium',
		variants[variant],
		className
	)}
>
	{@render children?.()}
</span>
