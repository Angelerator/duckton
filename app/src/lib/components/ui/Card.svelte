<script lang="ts">
	import { cn } from '$lib/utils';
	import type { Snippet } from 'svelte';

	let {
		class: className = '',
		children
	}: { class?: string; children?: Snippet } = $props();
</script>

<div class={cn('rounded-2xl border border-white/10 bg-white/[0.02] p-6', className)}>
	{@render children?.()}
</div>
