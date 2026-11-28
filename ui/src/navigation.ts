import { InjectionToken, type Type } from '@angular/core';
import type { Message } from './messages';

export interface Access {
  readonly installed: ReadonlySet<string>;
  readonly enabled: ReadonlySet<string>;
  readonly permissions: ReadonlySet<string>;
}
interface Restricted {
  readonly permissions?: readonly string[];
  readonly anyPermission?: readonly string[];
}
interface Entry extends Restricted {
  readonly id: string;
  readonly label: Message;
  readonly route: string;
  readonly children?: readonly Entry[];
}
export interface ModuleManifest extends Restricted {
  readonly schemaVersion: 1;
  readonly id: string;
  readonly label: Message;
  readonly icon: Type<unknown>;
  readonly route: string;
  readonly groups: readonly { readonly id: string; readonly label: Message; readonly entries: readonly Entry[] }[];
}
export interface PanelRegistration extends Restricted {
  readonly id: string;
  readonly title: Message;
  readonly component: Type<unknown>;
}
export interface ShellRegistration {
  readonly modules: readonly ModuleManifest[];
  readonly panels: readonly PanelRegistration[];
  readonly access: Access;
  readonly localSetup: boolean;
}
export const SHELL_REGISTRATION = new InjectionToken<ShellRegistration>('shell registration');

export function permits(access: Access, item: Restricted): boolean {
  return (item.permissions?.every(grant => access.permissions.has(grant)) ?? true)
    && (item.anyPermission?.some(grant => access.permissions.has(grant)) ?? true);
}

function visibleEntries(entries: readonly Entry[], access: Access): Entry[] {
  return entries.filter(entry => permits(access, entry))
    .map(entry => ({ ...entry, children: entry.children && visibleEntries(entry.children, access) }));
}

export function visibleModules(manifests: readonly ModuleManifest[], access: Access): ModuleManifest[] {
  return manifests.filter(module => access.installed.has(module.id) && access.enabled.has(module.id) && permits(access, module))
    .map(module => ({ ...module, groups: module.groups.map(group => ({
      ...group, entries: visibleEntries(group.entries, access)
    })).filter(group => group.entries.length) }));
}
