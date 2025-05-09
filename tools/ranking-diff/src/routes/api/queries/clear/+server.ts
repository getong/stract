import type { RequestEvent } from '@sveltejs/kit';
import { clearQueries } from '$lib/db';

export async function POST({}: RequestEvent): Promise<Response> {
	clearQueries();
	return new Response('OK');
}
