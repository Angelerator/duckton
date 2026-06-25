<script lang="ts">
	import { onMount, tick } from 'svelte';
	import { ScrollText } from '@lucide/svelte';
	import Card from '$lib/components/ui/Card.svelte';
	import { getLogs } from '$lib/ipc';

	let lines = $state<string[]>([]);
	let el = $state<HTMLDivElement | null>(null);
	let stick = $state(true);

	async function refresh() {
		try {
			lines = await getLogs();
			if (stick) {
				await tick();
				if (el) el.scrollTop = el.scrollHeight;
			}
		} catch (e) {
			console.error(e);
		}
	}

	function onScroll() {
		if (!el) return;
		stick = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
	}

	function lineClass(l: string) {
		if (/\bERROR\b/.test(l)) return 'text-red-300';
		if (/\bWARN\b/.test(l)) return 'text-amber-300';
		if (/\bINFO\b/.test(l)) return 'text-white/75';
		return 'text-white/55';
	}

	onMount(() => {
		refresh();
		const id = setInterval(refresh, 1500);
		return () => clearInterval(id);
	});
</script>

<div class="space-y-6">
	<div class="flex items-center justify-between">
		<div>
			<h1 class="text-2xl font-bold tracking-tight">Logs</h1>
			<p class="mt-1 text-sm text-white/50">Live node activity (most recent 500 lines).</p>
		</div>
		<ScrollText class="size-5 text-brand" />
	</div>

	<Card class="p-0">
		<div
			bind:this={el}
			onscroll={onScroll}
			class="h-[60vh] overflow-y-auto rounded-2xl bg-black/40 p-4 font-mono text-xs leading-relaxed"
		>
			{#if lines.length === 0}
				<div class="text-white/30">No log output yet…</div>
			{:else}
				{#each lines as line, i (i)}
					<div class={lineClass(line)}>{line}</div>
				{/each}
			{/if}
		</div>
	</Card>
</div>
