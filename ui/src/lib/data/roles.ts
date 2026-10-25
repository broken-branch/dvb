import type { Role, RoleFields } from './types';
export const roleFields = (role: Role): RoleFields => ({
  name: role.name,
  job: role.job,
  lane: role.lane,
  lead: role.lead,
  reports_to: role.reports_to,
  reviewed_by: role.reviewed_by,
  review_instructions: role.review_instructions,
  group: role.group,
  order: role.order,
});
