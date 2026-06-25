<script lang="ts">
	import '../app.css';
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import {
		LayoutDashboard,
		SlidersHorizontal,
		Coins,
		ScrollText,
		Play,
		Square,
		BookOpen,
		TriangleAlert
	} from '@lucide/svelte';
	import Button from '$lib/components/ui/Button.svelte';
	import { node } from '$lib/state.svelte';
	import { openExternal } from '$lib/ipc';

	const DOCS_URL = 'https://docs.duckton.com/';
	const GITHUB_URL = 'https://github.com/Angelerator/duckton';

	let { children } = $props();

	const nav = [
		{ href: '/', label: 'Overview', icon: LayoutDashboard },
		{ href: '/configuration', label: 'Configuration', icon: SlidersHorizontal },
		{ href: '/payments', label: 'Payments', icon: Coins },
		{ href: '/logs', label: 'Logs', icon: ScrollText }
	];

	const status = $derived(node.status);
	const running = $derived(status?.running ?? false);
	const isMainnet = $derived(status?.network === 'mainnet');
	const mainnetArmed = $derived(isMainnet && (status?.mainnet_confirmed ?? false));

	onMount(() => node.poll(2000));
</script>

<div class="flex min-h-screen bg-[#0a0a0b] text-white">
	<!-- Sidebar -->
	<aside
		class="fixed inset-y-0 left-0 z-30 hidden w-64 flex-col border-r border-white/10 bg-[#0a0a0b] lg:flex"
	>
		<div class="flex h-16 items-center gap-2.5 border-b border-white/10 px-5">
			<img src="/duckton-logo.png" alt="Duckton" class="size-8 rounded-[22%]" />
			<div class="leading-tight">
				<div class="text-sm font-semibold">Duckton</div>
				<div class="text-[10px] text-white/40">node</div>
			</div>
		</div>

		<nav class="flex-1 space-y-1 p-3">
			{#each nav as item (item.href)}
				{@const active = page.url.pathname === item.href}
				<a
					href={item.href}
					class="flex items-center gap-3 rounded-lg px-3 py-2 text-sm font-medium transition-colors {active
						? 'bg-brand/10 text-brand'
						: 'text-white/60 hover:bg-white/5 hover:text-white'}"
				>
					<item.icon class="size-4" />
					{item.label}
				</a>
			{/each}
		</nav>

		<div class="space-y-2 border-t border-white/10 px-4 py-3 text-xs">
			<div class="flex items-center justify-between">
				<span class="text-white/40">protocol</span>
				<span class="font-mono text-white/70">p2p/{status?.protocol_version ?? '—'}</span>
			</div>
			<div class="flex items-center justify-between">
				<span class="text-white/40">engine</span>
				<span class="font-mono text-white/70">duckdb {status?.engine_version ?? '—'}</span>
			</div>
			<button
				class="text-white/40 transition-colors hover:text-brand"
				onclick={() => openExternal(GITHUB_URL)}
			>
				github.com/Angelerator/duckton
			</button>
		</div>
	</aside>

	<!-- Main column -->
	<div class="flex min-w-0 flex-1 flex-col lg:pl-64">
		<header
			class="sticky top-0 z-20 flex h-16 items-center gap-3 border-b border-white/10 bg-[#0a0a0b]/80 px-5 backdrop-blur"
		>
			<div class="flex items-center gap-2">
				<span class="relative flex size-2.5">
					{#if running}
						<span
							class="absolute inline-flex size-full animate-ping rounded-full bg-emerald-400 opacity-70"
						></span>
					{/if}
					<span
						class="relative inline-flex size-2.5 rounded-full {running
							? 'bg-emerald-400'
							: 'bg-white/30'}"
					></span>
				</span>
				<span class="text-sm font-medium">{running ? 'Serving' : 'Stopped'}</span>
			</div>

			<span
				class="rounded-full border px-2.5 py-0.5 text-xs font-medium {isMainnet
					? 'border-amber-400/40 text-amber-300'
					: 'border-white/15 text-white/60'}"
			>
				{status?.network ?? 'testnet'}
			</span>

			<div class="ml-auto flex items-center gap-2">
				<Button variant="outline" size="sm" onclick={() => openExternal(DOCS_URL)}>
					<BookOpen class="size-3.5" /> Docs
				</Button>
				{#if running}
					<Button variant="outline" size="sm" disabled={node.busy} onclick={() => node.stop()}>
						<Square class="size-3.5" /> Stop
					</Button>
				{:else}
					<Button size="sm" disabled={node.busy} onclick={() => node.start()}>
						<Play class="size-3.5" /> Start node
					</Button>
				{/if}
			</div>
		</header>

		{#if mainnetArmed}
			<div
				class="flex items-center gap-2 border-b border-amber-400/30 bg-amber-400/10 px-5 py-2 text-xs text-amber-200"
			>
				<TriangleAlert class="size-4" />
				Mainnet is armed — actions move <span class="font-semibold">real funds</span>.
			</div>
		{/if}

		<main class="mx-auto w-full max-w-5xl flex-1 px-5 py-8">
			{@render children()}
		</main>
	</div>
</div>
