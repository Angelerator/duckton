<script lang="ts">
	import { cn } from '$lib/utils';
	import type { Snippet } from 'svelte';

	let {
		for: forId,
		class: className = '',
		children
	}: { for?: string; class?: string; children?: Snippet } = $props();
</script>

<label for={forId} class={cn('text-xs font-medium tracking-wide text-white/50 uppercase', className)}>
	{@render children?.()}
</label>
