<script lang="ts">
	import { cn } from '$lib/utils';
	import type { HTMLInputAttributes } from 'svelte/elements';

	interface Props extends Omit<HTMLInputAttributes, 'value'> {
		value?: string | number;
		class?: string;
	}

	let { value = $bindable(''), class: className = '', ...rest }: Props = $props();
</script>

<input
	bind:value
	class={cn(
		'h-10 w-full rounded-lg border border-white/10 bg-black/40 px-3 text-sm text-white outline-none transition placeholder:text-white/30 focus:border-brand/50',
		className
	)}
	{...rest}
/>
