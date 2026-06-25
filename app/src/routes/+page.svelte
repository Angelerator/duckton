<script lang="ts">
	import { Cpu, Server, Activity, Clock, ShieldCheck, Coins, Network, Copy, Check } from '@lucide/svelte';
	import Card from '$lib/components/ui/Card.svelte';
	import StatCard from '$lib/components/ui/StatCard.svelte';
	import Badge from '$lib/components/ui/Badge.svelte';
	import { node } from '$lib/state.svelte';
	import { humanBytes, num, shortId, uptime } from '$lib/format';

	const s = $derived(node.status);
	const running = $derived(s?.running ?? false);

	let copied = $state('');
	async function copy(text: string | null | undefined, key: string) {
		if (!text) return;
		await navigator.clipboard.writeText(text);
		copied = key;
		setTimeout(() => (copied = ''), 1200);
	}

	function row(label: string, value: string) {
		return { label, value };
	}
	const identityRows = $derived(
		s
			? [
					row('Node ID', s.node_id ?? '—'),
					row('Listen address', s.listen_addr ?? '—'),
					row('Configured bind', s.bind_addr),
					row('Data classes served', s.data_classes.join(', ') || '—')
				]
			: []
	);
</script>

<!-- Hero -->
<section class="bg-grid relative overflow-hidden rounded-3xl border border-white/10 p-8">
	<div
		class="pointer-events-none absolute -top-24 right-0 size-[420px] rounded-full opacity-20 blur-3xl"
		style="background: radial-gradient(circle, #ffd400, transparent 60%)"
	></div>
	<Badge variant="brand">
		<span class="size-1.5 rounded-full bg-brand"></span>
		{running ? 'Your machine is serving the grid' : 'Ready to join the grid'}
	</Badge>
	<h1 class="mt-4 max-w-2xl text-3xl font-bold tracking-tight md:text-4xl">
		Run your machine as a <span class="text-brand">Duckton</span> node.
	</h1>
	<p class="mt-3 max-w-2xl leading-relaxed text-white/60">
		Donate a slice of your RAM and CPU to a secure, peer-to-peer DuckDB compute grid over QUIC.
		Serve verified queries for others — and optionally earn, settled directly on TON.
	</p>
</section>

{#if node.error}
	<div class="mt-4 rounded-xl border border-red-400/30 bg-red-400/10 px-4 py-3 text-sm text-red-200">
		{node.error}
	</div>
{/if}

<!-- Live stats -->
<div class="mt-6 grid grid-cols-2 gap-4 md:grid-cols-4">
	<StatCard label="Status" value={running ? 'Serving' : 'Stopped'} sub={running ? 'accepting jobs' : 'press Start'}>
		{#snippet icon()}<Server />{/snippet}
	</StatCard>
	<StatCard label="Jobs served" value={num(s?.jobs_served ?? 0)} sub="this session">
		{#snippet icon()}<Activity />{/snippet}
	</StatCard>
	<StatCard label="Uptime" value={uptime(s?.uptime_secs ?? 0)} sub="since start">
		{#snippet icon()}<Clock />{/snippet}
	</StatCard>
	<StatCard label="Donated RAM" value={humanBytes(s?.memory_bytes ?? 0)} sub={`${s?.threads ?? 0} threads · ${s?.max_jobs ?? 0} jobs`}>
		{#snippet icon()}<Cpu />{/snippet}
	</StatCard>
</div>

<div class="mt-6 grid gap-4 md:grid-cols-2">
	<!-- Identity -->
	<Card>
		<div class="flex items-center gap-2">
			<ShieldCheck class="size-5 text-brand" />
			<h2 class="text-lg font-semibold">Node identity</h2>
		</div>
		<div class="mt-4 space-y-1">
			{#each identityRows as r (r.label)}
				<div class="flex items-center justify-between gap-3 border-t border-white/5 py-2 text-sm first:border-t-0">
					<span class="text-white/50">{r.label}</span>
					<button
						class="flex items-center gap-1.5 font-mono text-xs text-white/80 transition-colors hover:text-brand"
						title={r.value}
						onclick={() => copy(r.value, r.label)}
					>
						<span class="max-w-[180px] truncate">{r.label === 'Node ID' ? shortId(r.value, 10, 8) : r.value}</span>
						{#if copied === r.label}<Check class="size-3 text-emerald-400" />{:else}<Copy class="size-3 opacity-50" />{/if}
					</button>
				</div>
			{/each}
		</div>
	</Card>

	<!-- Economics summary -->
	<Card>
		<div class="flex items-center gap-2">
			<Coins class="size-5 text-brand" />
			<h2 class="text-lg font-semibold">Earnings & settlement</h2>
		</div>
		<div class="mt-4 space-y-1 text-sm">
			<div class="flex items-center justify-between border-t border-white/5 py-2 first:border-t-0">
				<span class="text-white/50">Payments</span>
				<Badge variant={s?.economics_enabled ? 'ok' : 'muted'}>
					{s?.economics_enabled ? `on · ${s?.settlement}` : 'free (off-chain)'}
				</Badge>
			</div>
			<div class="flex items-center justify-between border-t border-white/5 py-2">
				<span class="text-white/50">Network</span>
				<Badge variant={s?.network === 'mainnet' ? 'warn' : 'muted'}>{s?.network ?? 'testnet'}</Badge>
			</div>
			<div class="flex items-center justify-between border-t border-white/5 py-2">
				<span class="text-white/50">Wallet</span>
				<span class="font-mono text-xs text-white/80">{shortId(s?.wallet_address)}</span>
			</div>
			<div class="flex items-center justify-between border-t border-white/5 py-2">
				<span class="text-white/50">Rate</span>
				<span class="font-mono text-xs text-white/80">{s?.unit_price ?? 0} TON / unit</span>
			</div>
		</div>
		<a
			href="/payments"
			class="mt-4 inline-flex items-center gap-1.5 text-sm font-semibold text-brand hover:underline"
		>
			Configure payments <Network class="size-4" />
		</a>
	</Card>
</div>
