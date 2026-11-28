export async function readBoundedJson(response: Response, limit: number): Promise<unknown> {
  const reader = response.body?.getReader();
  if (!reader) throw new Error('Empty JSON response');
  const decoder = new TextDecoder('utf-8', { fatal: true });
  let size = 0, text = '';
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) return JSON.parse(text + decoder.decode()) as unknown;
      size += value.byteLength;
      if (size > limit) throw new Error('JSON response exceeds its bound');
      text += decoder.decode(value, { stream: true });
    }
  } catch (error) {
    await reader.cancel().catch(() => {});
    throw error;
  } finally {
    reader.releaseLock();
  }
}
