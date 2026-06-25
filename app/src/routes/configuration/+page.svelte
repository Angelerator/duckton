<script lang="ts">
	import { onMount } from 'svelte';
	import { Cpu, Network, ShieldCheck, Save, RotateCw, Check } from '@lucide/svelte';
	import Card from '$lib/components/ui/Card.svelte';
	import Field from '$lib/components/ui/Field.svelte';
	import Input from '$lib/components/ui/Input.svelte';
	import Switch from '$lib/components/ui/Switch.svelte';
	import Select from '$lib/components/ui/Select.svelte';
	import Button from '$lib/components/ui/Button.svelte';
	import { getConfig, saveConfig, type ConfigView } from '$lib/ipc';
	import { node } from '$lib/state.svelte';
	import { bytesToGib, gibToBytes } from '$lib/format';

	let cfg = $state<ConfigView | null>(null);
	let memGib = $state(4);
	let perJobGib = $state(1);
	let advertised = $state('');
	let bootstrapText = $state('');
	let saving = $state(false);
	let saved = $state(false);
	let errMsg = $state('');

	const CLASSES = [
		{ id: 'public', label: 'Public', desc: 'open jobs, no stake required' },
		{ id: 'internal', label: 'Internal', desc: 'company / grouped jobs' },
		{ id: 'sensitive', label: 'Sensitive', desc: 'requires attested hardware (L2)' }
	];

	onMount(load);

	async function load() {
		cfg = await getConfig();
		memGib = bytesToGib(cfg.memory_bytes);
		perJobGib = bytesToGib(cfg.per_job_memory_bytes);
		advertised = cfg.advertised_addr ?? '';
		bootstrapText = cfg.bootstrap.join('\n');
	}

	function hasClass(c: string) {
		return cfg?.data_classes.includes(c) ?? false;
	}
	function toggleClass(c: string, on: boolean) {
		if (!cfg) return;
		const set = new Set(cfg.data_classes);
		if (on) set.add(c);
		else set.delete(c);
		cfg.data_classes = CLASSES.map((x) => x.id).filter((x) => set.has(x));
	}

	async function save(restart = false) {
		if (!cfg) return;
		saving = true;
		saved = false;
		errMsg = '';
		cfg.memory_bytes = gibToBytes(memGib);
		cfg.per_job_memory_bytes = gibToBytes(perJobGib);
		cfg.advertised_addr = advertised.trim() ? advertised.trim() : null;
		cfg.bootstrap = bootstrapText
			.split('\n')
			.map((s) => s.trim())
			.filter(Boolean);
		try {
			await saveConfig(cfg);
			saved = true;
			if (restart) await node.start();
			else await node.refresh();
			setTimeout(() => (saved = false), 1500);
		} catch (e) {
			errMsg = String(e);
		} finally {
			saving = false;
		}
	}
</script>

<div class="space-y-6">
	<div>
		<h1 class="text-2xl font-bold tracking-tight">Configuration</h1>
		<p class="mt-1 text-sm text-white/50">
			How much of this machine you donate, what it serves, and how it joins the grid. Changes apply
			on the next node start.
		</p>
	</div>

	{#if errMsg}
		<div class="rounded-xl border border-red-400/30 bg-red-400/10 px-4 py-3 text-sm text-red-200">
			{errMsg}
		</div>
	{/if}

	{#if cfg}
		<!-- Resources -->
		<Card>
			<div class="flex items-center gap-2">
				<Cpu class="size-5 text-brand" />
				<h2 class="text-lg font-semibold">Donated resources</h2>
			</div>
			<div class="mt-5 grid gap-5 md:grid-cols-2">
				<Field label={`Memory — ${memGib} GiB`} hint="Total RAM this node donates to serving jobs.">
					<input type="range" min="1" max="64" step="1" bind:value={memGib} class="w-full accent-[#ffd400]" />
				</Field>
				<Field label={`Per-job memory — ${perJobGib} GiB`} hint="Default lease handed to a single job.">
					<input type="range" min="1" max="32" step="1" bind:value={perJobGib} class="w-full accent-[#ffd400]" />
				</Field>
				<Field label="Threads" hint="CPU threads donated.">
					<Input type="number" min="1" max="256" bind:value={cfg.threads} />
				</Field>
				<Field label="Max concurrent jobs" hint="Jobs admitted at once.">
					<Input type="number" min="1" max="64" bind:value={cfg.max_jobs} />
				</Field>
			</div>

			<div class="mt-6">
				<div class="text-xs font-medium tracking-wide text-white/50 uppercase">Data classes served</div>
				<div class="mt-3 grid gap-3 md:grid-cols-3">
					{#each CLASSES as c (c.id)}
						<label class="flex items-start justify-between gap-3 rounded-xl border border-white/10 bg-white/[0.02] p-3">
							<span>
								<span class="text-sm font-medium">{c.label}</span>
								<span class="mt-0.5 block text-xs text-white/40">{c.desc}</span>
							</span>
							<Switch checked={hasClass(c.id)} onchange={(v) => toggleClass(c.id, v)} />
						</label>
					{/each}
				</div>
			</div>
		</Card>

		<!-- Networking -->
		<Card>
			<div class="flex items-center gap-2">
				<Network class="size-5 text-brand" />
				<h2 class="text-lg font-semibold">Networking & discovery</h2>
			</div>
			<div class="mt-5 grid gap-5 md:grid-cols-2">
				<Field label="QUIC bind address" hint="host:port the node listens on (0.0.0.0 = all interfaces).">
					<Input bind:value={cfg.bind_addr} placeholder="0.0.0.0:9494" />
				</Field>
				<Field label="Advertised address (optional)" hint="Externally reachable host:port if you are behind NAT.">
					<Input bind:value={advertised} placeholder="203.0.113.10:9494" />
				</Field>
			</div>
			<Field class="mt-5" label="Bootstrap seeds" hint="One host:port per line. Entry points used to join the swarm (never in the data path).">
				<textarea
					bind:value={bootstrapText}
					rows="3"
					class="w-full rounded-lg border border-white/10 bg-black/40 px-3 py-2 font-mono text-xs text-white outline-none transition placeholder:text-white/30 focus:border-brand/50"
					placeholder="seed.duckton.com:9494"
				></textarea>
			</Field>
			<div class="mt-5 grid gap-3 md:grid-cols-2">
				<label class="flex items-center justify-between rounded-xl border border-white/10 bg-white/[0.02] p-3 text-sm">
					mDNS LAN discovery <Switch bind:checked={cfg.mdns} />
				</label>
				<label class="flex items-center justify-between rounded-xl border border-white/10 bg-white/[0.02] p-3 text-sm">
					AutoNAT reachability <Switch bind:checked={cfg.autonat} />
				</label>
				<label class="flex items-center justify-between rounded-xl border border-white/10 bg-white/[0.02] p-3 text-sm">
					Relay client (NAT traversal) <Switch bind:checked={cfg.relay_client} />
				</label>
				<label class="flex items-center justify-between rounded-xl border border-white/10 bg-white/[0.02] p-3 text-sm">
					Volunteer as relay <Switch bind:checked={cfg.act_as_relay} />
				</label>
			</div>
		</Card>

		<!-- Security -->
		<Card>
			<div class="flex items-center gap-2">
				<ShieldCheck class="size-5 text-brand" />
				<h2 class="text-lg font-semibold">Identity & posture</h2>
			</div>
			<div class="mt-5 grid gap-5 md:grid-cols-2">
				<Field label="Identity pinning" hint="TOFU (trust-on-first-use) or a strict allowlist.">
					<Select
						bind:value={cfg.pinning_mode}
						options={[
							{ value: 'tofu', label: 'TOFU — trust on first use' },
							{ value: 'allowlist', label: 'Allowlist — pinned peers only' }
						]}
					/>
				</Field>
				<Field label="Closure posture" hint="Private requires allowlist + token group enforcement.">
					<Select
						bind:value={cfg.security_mode}
						options={[
							{ value: 'public', label: 'Public — open grid' },
							{ value: 'private', label: 'Private — closed company grid' }
						]}
					/>
				</Field>
			</div>
		</Card>

		<div class="flex items-center gap-3">
			<Button disabled={saving} onclick={() => save(true)}>
				<RotateCw class="size-4" /> Save & restart node
			</Button>
			<Button variant="outline" disabled={saving} onclick={() => save(false)}>
				{#if saved}<Check class="size-4 text-emerald-400" /> Saved{:else}<Save class="size-4" /> Save only{/if}
			</Button>
		</div>
	{:else}
		<div class="text-sm text-white/40">Loading configuration…</div>
	{/if}
</div>
