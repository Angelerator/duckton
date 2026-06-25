<script lang="ts">
	import { cn } from '$lib/utils';
	import { ChevronDown } from '@lucide/svelte';

	type Option = { value: string; label: string };

	let {
		value = $bindable(''),
		options = [],
		disabled = false,
		class: className = '',
		onchange
	}: {
		value?: string;
		options?: Option[];
		disabled?: boolean;
		class?: string;
		onchange?: (v: string) => void;
	} = $props();

	function handle(e: Event) {
		value = (e.target as HTMLSelectElement).value;
		onchange?.(value);
	}
</script>

<div class={cn('relative', className)}>
	<select
		bind:value
		onchange={handle}
		{disabled}
		class="h-10 w-full appearance-none rounded-lg border border-white/10 bg-black/40 px-3 pr-9 text-sm text-white outline-none transition focus:border-brand/50 disabled:opacity-50"
	>
		{#each options as opt (opt.value)}
			<option value={opt.value} class="bg-[#0a0a0b] text-white">{opt.label}</option>
		{/each}
	</select>
	<ChevronDown class="pointer-events-none absolute top-1/2 right-3 size-4 -translate-y-1/2 text-white/40" />
</div>
