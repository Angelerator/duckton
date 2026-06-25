<script lang="ts">
	import { cn } from '$lib/utils';

	let {
		checked = $bindable(false),
		disabled = false,
		label = 'Toggle',
		onchange
	}: { checked?: boolean; disabled?: boolean; label?: string; onchange?: (v: boolean) => void } =
		$props();

	function toggle() {
		if (disabled) return;
		checked = !checked;
		onchange?.(checked);
	}
</script>

<button
	type="button"
	role="switch"
	aria-checked={checked}
	aria-label={label}
	{disabled}
	onclick={toggle}
	class={cn(
		'relative inline-flex h-6 w-11 shrink-0 cursor-pointer items-center rounded-full transition-colors disabled:cursor-not-allowed disabled:opacity-50',
		checked ? 'bg-brand' : 'bg-white/15'
	)}
>
	<span
		class={cn(
			'inline-block size-5 transform rounded-full bg-white shadow transition-transform',
			checked ? 'translate-x-[22px]' : 'translate-x-[2px]'
		)}
	></span>
</button>
