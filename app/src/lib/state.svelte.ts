import { getStatus, startNode, stopNode, type NodeStatus } from './ipc';

/** App-wide reactive node status, polled on an interval. */
class NodeStore {
	status = $state<NodeStatus | null>(null);
	error = $state<string | null>(null);
	busy = $state(false);

	async refresh() {
		try {
			this.status = await getStatus();
			this.error = null;
		} catch (e) {
			this.error = String(e);
		}
	}

	async start() {
		this.busy = true;
		try {
			this.status = await startNode();
			this.error = null;
		} catch (e) {
			this.error = String(e);
		} finally {
			this.busy = false;
		}
	}

	async stop() {
		this.busy = true;
		try {
			this.status = await stopNode();
			this.error = null;
		} catch (e) {
			this.error = String(e);
		} finally {
			this.busy = false;
		}
	}

	/** Begin polling; returns a stop function. */
	poll(intervalMs = 2000): () => void {
		this.refresh();
		const id = setInterval(() => this.refresh(), intervalMs);
		return () => clearInterval(id);
	}
}

export const node = new NodeStore();
