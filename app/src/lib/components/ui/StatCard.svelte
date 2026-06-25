<script lang="ts">
	import type { Snippet } from 'svelte';

	let {
		label,
		value,
		sub = '',
		icon
	}: { label: string; value: string; sub?: string; icon?: Snippet } = $props();
</script>

<div class="rounded-xl border border-white/10 bg-white/[0.02] p-4">
	<div class="flex items-center justify-between">
		<span class="text-xs font-medium text-white/50">{label}</span>
		<span class="text-brand [&_svg]:size-4">{@render icon?.()}</span>
	</div>
	<div class="mt-2 text-2xl font-semibold tabular-nums text-white">{value}</div>
	{#if sub}<div class="mt-1 text-xs text-white/40">{sub}</div>{/if}
</div>
