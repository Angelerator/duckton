<script lang="ts">
	import {
		Coins,
		Wallet,
		FileText,
		Tags,
		Landmark,
		TriangleAlert,
		ExternalLink,
		Check
	} from '@lucide/svelte';
	import Card from '$lib/components/ui/Card.svelte';
	import Field from '$lib/components/ui/Field.svelte';
	import Input from '$lib/components/ui/Input.svelte';
	import Switch from '$lib/components/ui/Switch.svelte';
	import Select from '$lib/components/ui/Select.svelte';
	import Button from '$lib/components/ui/Button.svelte';
	import Badge from '$lib/components/ui/Badge.svelte';
	import {
		setEconomics,
		setWallet,
		setContracts,
		setPricing,
		stake,
		unstake,
		openExternal,
		type ActionResult
	} from '$lib/ipc';
	import { node } from '$lib/state.svelte';

	let loaded = $state(false);
	let enabled = $state(false);
	let settlement = $state('noop');
	let net = $state('testnet');
	let mainnetConfirm = $state(false);
	let defaultPayment = $state('auto');
	let feeRecipient = $state('');

	let walletAddress = $state('');
	let mnemonic = $state('');
	let apiKey = $state('');

	let gp = $state('');
	let sv = $state('');
	let je = $state('');
	let ra = $state('');

	let unitPrice = $state(0);
	let maxBid = $state(0);
	let stakeAmount = $state(100);

	let savedKey = $state('');
	let errMsg = $state('');
	let action = $state<ActionResult | null>(null);
	let working = $state('');

	// Prefill once from the live status (the active network's settings).
	$effect(() => {
		const s = node.status;
		if (s && !loaded) {
			enabled = s.economics_enabled;
			settlement = ['noop', 'mock', 'ton'].includes(s.settlement) ? s.settlement : 'ton';
			net = s.network;
			mainnetConfirm = s.mainnet_confirmed;
			defaultPayment = s.default_payment;
			feeRecipient = s.fee_recipient ?? '';
			walletAddress = s.wallet_address ?? '';
			gp = s.global_params ?? '';
			sv = s.stake_vault ?? '';
			je = s.job_escrow ?? '';
			ra = s.record_anchor ?? '';
			unitPrice = s.unit_price;
			maxBid = s.max_bid;
			loaded = true;
		}
	});

	const onMainnet = $derived(net === 'mainnet');
	const onchain = $derived(enabled && settlement === 'ton');
	const canStake = $derived(onchain && !!sv && (!onMainnet || mainnetConfirm));

	function flash(key: string) {
		savedKey = key;
		setTimeout(() => (savedKey = ''), 1500);
	}

	async function run<T>(key: string, fn: () => Promise<T>) {
		working = key;
		errMsg = '';
		try {
			await fn();
			flash(key);
		} catch (e) {
			errMsg = String(e);
		} finally {
			working = '';
		}
	}

	const saveEconomics = () =>
		run('econ', async () => {
			await setEconomics({
				enabled,
				settlement,
				network: net,
				mainnet_confirm: mainnetConfirm,
				fee_recipient: feeRecipient || null,
				default_payment: defaultPayment
			});
			await node.refresh();
		});

	const saveWallet = () =>
		run('wallet', async () => {
			await setWallet({
				network: net,
				address: walletAddress || null,
				mnemonic: mnemonic || null,
				api_key: apiKey || null
			});
			mnemonic = '';
			apiKey = '';
			await node.refresh();
		});

	const saveContracts = () =>
		run('contracts', async () => {
			await setContracts({
				network: net,
				global_params: gp || null,
				stake_vault: sv || null,
				job_escrow: je || null,
				record_anchor: ra || null
			});
			await node.refresh();
		});

	const savePricing = () =>
		run('pricing', async () => {
			await setPricing({ unit_price: Number(unitPrice), max_bid: Number(maxBid) });
			await node.refresh();
		});

	async function doStake(kind: 'stake' | 'unstake') {
		working = kind;
		action = null;
		errMsg = '';
		try {
			action = kind === 'stake' ? await stake(Number(stakeAmount)) : await unstake(Number(stakeAmount));
		} catch (e) {
			errMsg = String(e);
		} finally {
			working = '';
		}
	}

	function txLink(tx: string) {
		const ex = node.status?.explorer ?? 'testnet.tonviewer.com';
		openExternal(`https://${ex}/transaction/${tx}`);
	}
</script>

<div class="space-y-6">
	<div>
		<h1 class="text-2xl font-bold tracking-tight">Payments</h1>
		<p class="mt-1 text-sm text-white/50">
			Stay free and fully off-chain, or earn on TON. Configure the settlement rail, your wallet,
			the on-chain contracts, and your rate — on <span class="font-medium text-white/70">testnet</span>
			or <span class="font-medium text-white/70">mainnet</span>. Secrets are written to local
			<span class="font-mono text-white/70">0600</span> files, never stored in the config.
		</p>
	</div>

	{#if errMsg}
		<div class="rounded-xl border border-red-400/30 bg-red-400/10 px-4 py-3 text-sm text-red-200">
			{errMsg}
		</div>
	{/if}

	<!-- Settlement rail -->
	<Card>
		<div class="flex items-center gap-2">
			<Coins class="size-5 text-brand" />
			<h2 class="text-lg font-semibold">Settlement rail</h2>
		</div>

		<label class="mt-5 flex items-center justify-between rounded-xl border border-white/10 bg-white/[0.02] p-4">
			<span>
				<span class="text-sm font-medium">Enable on-chain economics</span>
				<span class="mt-0.5 block text-xs text-white/40">
					Off = every job is free and touches no chain. On = paid jobs settle via escrow.
				</span>
			</span>
			<Switch bind:checked={enabled} />
		</label>

		<div class="mt-5 grid gap-5 md:grid-cols-2">
			<Field label="Settlement" hint="noop/mock are off-chain; ton settles on The Open Network.">
				<Select
					bind:value={settlement}
					options={[
						{ value: 'noop', label: 'noop — no settlement' },
						{ value: 'mock', label: 'mock — simulate paid flow (no funds)' },
						{ value: 'ton', label: 'ton — live on-chain escrow' }
					]}
				/>
			</Field>
			<Field label="Network" hint="Mainnet moves real funds and needs explicit confirmation.">
				<Select
					bind:value={net}
					options={[
						{ value: 'testnet', label: 'Testnet — safe, free test TON' },
						{ value: 'mainnet', label: 'Mainnet — real funds' }
					]}
				/>
			</Field>
			<Field label="Default payment" hint="Per-call overridable.">
				<Select
					bind:value={defaultPayment}
					options={[
						{ value: 'auto', label: 'auto — public free, private paid' },
						{ value: 'free', label: 'free' },
						{ value: 'paid', label: 'paid' }
					]}
				/>
			</Field>
			<Field label="Fee recipient (treasury)" hint="Required once paid settlement is enabled.">
				<Input bind:value={feeRecipient} placeholder="EQ… / kQ…" class="font-mono text-xs" />
			</Field>
		</div>

		{#if onMainnet}
			<label class="mt-5 flex items-center justify-between rounded-xl border border-amber-400/30 bg-amber-400/10 p-4">
				<span class="flex items-start gap-2">
					<TriangleAlert class="mt-0.5 size-4 text-amber-300" />
					<span>
						<span class="text-sm font-medium text-amber-200">I understand mainnet moves real funds</span>
						<span class="mt-0.5 block text-xs text-amber-200/70">
							Required to arm mainnet. Without it, on-chain actions fail closed.
						</span>
					</span>
				</span>
				<Switch bind:checked={mainnetConfirm} />
			</label>
		{/if}

		<div class="mt-5 flex flex-wrap items-center gap-3">
			<Button disabled={working === 'econ'} onclick={saveEconomics}>
				{#if savedKey === 'econ'}<Check class="size-4 text-emerald-900" /> Saved{:else}Save settlement{/if}
			</Button>
			<span class="text-xs text-white/40">RPC: <span class="font-mono">{node.status?.rpc_endpoint}</span></span>
		</div>
		<p class="mt-3 text-xs text-white/35">
			Staking applies immediately. Changes to the rail used while <em>serving</em> paid jobs take
			effect when you restart the node (top-right).
		</p>
	</Card>

	<!-- Wallet -->
	<Card>
		<div class="flex items-center justify-between">
			<div class="flex items-center gap-2">
				<Wallet class="size-5 text-brand" />
				<h2 class="text-lg font-semibold">Wallet</h2>
			</div>
			<Badge variant={onMainnet ? 'warn' : 'muted'}>{net}</Badge>
		</div>
		<div class="mt-5 space-y-5">
			<Field label="Wallet address" hint="Safe to display. Where payouts are received.">
				<Input bind:value={walletAddress} placeholder="EQ… / kQ…" class="font-mono text-xs" />
			</Field>
			<Field label="Mnemonic" hint="24 words. Written to a 0600 file on this machine — never stored in config or sent anywhere.">
				<Input type="password" bind:value={mnemonic} placeholder="leave blank to keep the existing secret" />
			</Field>
			<Field label="Toncenter API key (optional)" hint="Higher RPC rate limits. Stored as a 0600 file reference.">
				<Input type="password" bind:value={apiKey} placeholder="optional" />
			</Field>
		</div>
		<div class="mt-5">
			<Button disabled={working === 'wallet'} onclick={saveWallet}>
				{#if savedKey === 'wallet'}<Check class="size-4 text-emerald-900" /> Saved{:else}Save wallet{/if}
			</Button>
		</div>
	</Card>

	<!-- Contracts -->
	<Card>
		<div class="flex items-center justify-between">
			<div class="flex items-center gap-2">
				<FileText class="size-5 text-brand" />
				<h2 class="text-lg font-semibold">Contracts</h2>
			</div>
			<Badge variant={onMainnet ? 'warn' : 'muted'}>{net}</Badge>
		</div>
		<div class="mt-5 grid gap-5 md:grid-cols-2">
			<Field label="GlobalParams" hint="Platform-wide economic params (stable address).">
				<Input bind:value={gp} placeholder="kQ…" class="font-mono text-xs" />
			</Field>
			<Field label="StakeVault" hint="Provider staking contract.">
				<Input bind:value={sv} placeholder="kQ…" class="font-mono text-xs" />
			</Field>
			<Field label="JobEscrow" hint="Per-job escrow factory/template.">
				<Input bind:value={je} placeholder="kQ…" class="font-mono text-xs" />
			</Field>
			<Field label="RecordAnchor" hint="Epoch Merkle-root anchoring.">
				<Input bind:value={ra} placeholder="kQ…" class="font-mono text-xs" />
			</Field>
		</div>
		<div class="mt-5">
			<Button disabled={working === 'contracts'} onclick={saveContracts}>
				{#if savedKey === 'contracts'}<Check class="size-4 text-emerald-900" /> Saved{:else}Save contracts{/if}
			</Button>
		</div>
	</Card>

	<!-- Pricing -->
	<Card>
		<div class="flex items-center gap-2">
			<Tags class="size-5 text-brand" />
			<h2 class="text-lg font-semibold">Pricing</h2>
		</div>
		<div class="mt-5 grid gap-5 md:grid-cols-2">
			<Field label="Unit price (whole TON)" hint="Your advertised rate per reference unit. 0 = free.">
				<Input type="number" min="0" bind:value={unitPrice} />
			</Field>
			<Field label="Max bid (whole TON)" hint="Budget cap as a requester. 0 = no cap.">
				<Input type="number" min="0" bind:value={maxBid} />
			</Field>
		</div>
		<div class="mt-5">
			<Button disabled={working === 'pricing'} onclick={savePricing}>
				{#if savedKey === 'pricing'}<Check class="size-4 text-emerald-900" /> Saved{:else}Save pricing{/if}
			</Button>
		</div>
	</Card>

	<!-- Staking -->
	<Card>
		<div class="flex items-center gap-2">
			<Landmark class="size-5 text-brand" />
			<h2 class="text-lg font-semibold">Staking</h2>
		</div>
		<p class="mt-2 text-sm text-white/50">
			Bond TON in the StakeVault for eligibility and ranking on paid jobs. Broadcasts a live
			on-chain transaction from your wallet.
		</p>
		<div class="mt-5 flex flex-wrap items-end gap-4">
			<Field label="Amount (whole TON)" class="w-40">
				<Input type="number" min="1" bind:value={stakeAmount} />
			</Field>
			<Button disabled={!canStake || working === 'stake'} onclick={() => doStake('stake')}>
				{working === 'stake' ? 'Broadcasting…' : 'Stake'}
			</Button>
			<Button
				variant="outline"
				disabled={!canStake || working === 'unstake'}
				onclick={() => doStake('unstake')}
			>
				{working === 'unstake' ? 'Broadcasting…' : 'Unbond'}
			</Button>
		</div>
		{#if !canStake}
			<p class="mt-3 text-xs text-white/35">
				Enable on-chain economics (ton), set a StakeVault contract{onMainnet
					? ', confirm mainnet,'
					: ''} and configure a wallet to stake.
			</p>
		{/if}
		{#if action}
			<div
				class="mt-4 rounded-xl border px-4 py-3 text-sm {action.ok
					? 'border-emerald-400/30 bg-emerald-400/10 text-emerald-200'
					: 'border-red-400/30 bg-red-400/10 text-red-200'}"
			>
				<div>{action.message}</div>
				{#if action.tx}
					<button
						class="mt-1.5 inline-flex items-center gap-1.5 font-mono text-xs text-brand hover:underline"
						onclick={() => txLink(action!.tx!)}
					>
						{action.tx.slice(0, 24)}… <ExternalLink class="size-3" />
					</button>
				{/if}
			</div>
		{/if}
	</Card>
</div>
