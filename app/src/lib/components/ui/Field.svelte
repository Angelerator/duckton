<script lang="ts">
	import { cn } from '$lib/utils';
	import Label from './Label.svelte';
	import type { Snippet } from 'svelte';

	let {
		label,
		hint,
		class: className = '',
		children
	}: { label?: string; hint?: string; class?: string; children?: Snippet } = $props();
</script>

<div class={cn('space-y-1.5', className)}>
	{#if label}<Label>{label}</Label>{/if}
	{@render children?.()}
	{#if hint}<p class="text-xs leading-relaxed text-white/35">{hint}</p>{/if}
</div>
