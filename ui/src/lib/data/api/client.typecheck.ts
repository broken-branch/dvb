// Include fixed engine JSON in contract typechecking without a runtime dependency.
import type {} from '../../../../tests/support/engine-fixtures';
import type { createClient } from './client';

// Included in tsconfig.app.json so CI's tsc build checks these calls; never run.
export function checkOperationBodies(client: ReturnType<typeof createClient>) {
  void client.call('/version', 'get');
  void client.call('/projects/{id}/pause', 'post');
  // @ts-expect-error The generated GET operation has no request body.
  void client.call('/version', 'get', { body: { unexpected: true } });
  // @ts-expect-error Bodyless POST operations must reject request bodies too.
  void client.call('/projects/{id}/pause', 'post', { body: { unexpected: true } });

  void client.call('/tasks/{id}/diff', 'get', { params: { id: 'a/b' } });
  // @ts-expect-error Path parameter names come from the generated operation.
  void client.call('/tasks/{id}/diff', 'get', { params: { task: 'a/b' } });
  // @ts-expect-error A bodyless route has no path parameters either.
  void client.call('/version', 'get', { params: { id: 'a/b' } });

  void client.call('/logins/{vendor}', 'post');
  void client.call('/logins/{vendor}', 'post', { body: null });
  void client.call('/logins/{vendor}', 'post', { body: { run: 'run', code: 'code' } });
  void client.call('/logins/{vendor}', 'post', { body: { run: 'run', code: '' } });
  // @ts-expect-error Login completion still requires both generated fields.
  void client.call('/logins/{vendor}', 'post', { body: { run: 'run' } });
}
