<script lang="ts">
	import { cn } from '$lib/utils';
	import type { Snippet } from 'svelte';
	import type { HTMLButtonAttributes } from 'svelte/elements';

	type Variant = 'brand' | 'outline' | 'ghost' | 'destructive' | 'secondary';
	type Size = 'sm' | 'md' | 'lg' | 'icon';

	interface Props extends HTMLButtonAttributes {
		variant?: Variant;
		size?: Size;
		class?: string;
		children?: Snippet;
	}

	let { variant = 'brand', size = 'md', class: className = '', children, ...rest }: Props = $props();

	const base =
		'inline-flex items-center justify-center gap-2 rounded-lg font-medium transition-all disabled:opacity-50 disabled:pointer-events-none focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand/50 [&_svg]:size-4 [&_svg]:shrink-0';

	const variants: Record<Variant, string> = {
		brand: 'bg-brand text-brand-foreground font-semibold hover:brightness-105 active:brightness-95',
		outline: 'border border-white/15 text-white hover:bg-white/5',
		ghost: 'text-white/70 hover:text-white hover:bg-white/5',
		destructive: 'bg-red-500/90 text-white hover:bg-red-500',
		secondary: 'bg-white/10 text-white hover:bg-white/15'
	};

	const sizes: Record<Size, string> = {
		sm: 'h-8 px-3 text-xs',
		md: 'h-10 px-4 text-sm',
		lg: 'h-11 px-5 text-sm',
		icon: 'size-9'
	};
</script>

<button class={cn(base, variants[variant], sizes[size], className)} {...rest}>
	{@render children?.()}
</button>
