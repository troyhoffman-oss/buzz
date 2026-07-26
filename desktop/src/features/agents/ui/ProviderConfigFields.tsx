import * as React from "react";

import { Input } from "@/shared/ui/input";

/// Coerce string config values to their schema-declared types (number, boolean).
/// Providers receive JSON — sending "3" instead of 3 for an integer field breaks
/// typed config parsing on the provider side.
export function coerceConfigValues(
  config: Record<string, string>,
  schema: Record<string, unknown> | undefined,
): Record<string, unknown> {
  if (!schema) return { ...config };
  const properties = ((schema as Record<string, unknown>)?.properties ??
    {}) as Record<string, Record<string, unknown>>;
  const result: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(config)) {
    const prop = properties[key] as Record<string, unknown> | undefined;
    const schemaType = prop?.type;
    if ((schemaType === "integer" || schemaType === "number") && value !== "") {
      const num = Number(value);
      result[key] = Number.isNaN(num) ? value : num;
    } else if (schemaType === "boolean") {
      result[key] = value === "true";
    } else {
      result[key] = value;
    }
  }
  return result;
}

/** Sentinel option value for "the answer is not in this list". */
export const PROVIDER_CONFIG_OTHER_VALUE = "__other__";

export type ProviderConfigChoice = { label: string; value: string };

/**
 * Suggested values a provider decorated a config property with.
 *
 * JSON Schema `oneOf: [{ const, title }]` is the provider's channel for
 * enumerating what it already knows about the user's environment (the SSH
 * provider fills it from the local Tailscale peer list). It is a SUGGESTION,
 * never a constraint: the desktop keeps a free-text escape hatch below, and a
 * provider that omits the decoration gets exactly the plain text field it got
 * before. Nothing here knows what a tailnet is — the decoration is generic and
 * every provider can use it.
 *
 * Returns `null` — not `[]` — when there is nothing usable to offer, so the
 * caller renders the unchanged Input rather than an empty dropdown.
 */
export function providerConfigChoices(
  prop: Record<string, unknown>,
): ProviderConfigChoice[] | null {
  const oneOf = prop.oneOf;
  if (!Array.isArray(oneOf)) return null;
  const choices: ProviderConfigChoice[] = [];
  for (const entry of oneOf) {
    if (typeof entry !== "object" || entry === null) continue;
    const { const: value, title } = entry as Record<string, unknown>;
    if (typeof value !== "string" || value.length === 0) continue;
    choices.push({
      label: typeof title === "string" && title.trim() ? title : value,
      value,
    });
  }
  return choices.length > 0 ? choices : null;
}

/**
 * Whether the field is answering with free text rather than a listed choice.
 *
 * True when the user asked for it, and also when the current value is simply
 * not in the list: a config carried over from before the list existed (or a
 * host that has since left the tailnet) must stay editable instead of silently
 * reading as "nothing selected".
 */
export function usesProviderConfigFreeText({
  choices,
  explicitlyOther,
  value,
}: {
  choices: ProviderConfigChoice[] | null;
  explicitlyOther: boolean;
  value: string;
}): boolean {
  if (choices === null) return true;
  if (explicitlyOther) return true;
  return value !== "" && !choices.some((choice) => choice.value === value);
}

export function ProviderConfigFields({
  schema,
  config,
  onChange,
}: {
  schema: Record<string, unknown>;
  config: Record<string, string>;
  onChange: (config: Record<string, string>) => void;
}) {
  // Keys the user explicitly moved to free text. Derived staleness (a value
  // that is simply absent from the list) is computed instead of stored, so a
  // re-probe that starts offering the typed value can adopt it.
  const [otherKeys, setOtherKeys] = React.useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const properties = (schema as Record<string, unknown>)?.properties ?? {};
  const required = new Set<string>(
    ((schema as Record<string, unknown>)?.required as string[]) ?? [],
  );

  const entries = Object.entries(properties) as [
    string,
    Record<string, unknown>,
  ][];

  if (entries.length === 0) {
    return null;
  }

  function setOther(key: string, isOther: boolean) {
    setOtherKeys((previous) => {
      const next = new Set(previous);
      if (isOther) next.add(key);
      else next.delete(key);
      return next;
    });
  }

  return (
    <div className="space-y-3">
      {entries.map(([key, prop]) => {
        const choices = providerConfigChoices(prop);
        const description =
          typeof prop.description === "string" ? prop.description : "";
        const value =
          config[key] ?? (typeof prop.default === "string" ? prop.default : "");
        const freeText = usesProviderConfigFreeText({
          choices,
          explicitlyOther: otherKeys.has(key),
          value,
        });
        const textInput = (
          <Input
            {...(choices ? { "aria-label": `Other ${key}` } : {})}
            id={choices ? `provider-cfg-${key}-other` : `provider-cfg-${key}`}
            onChange={(e) => onChange({ ...config, [key]: e.target.value })}
            placeholder={description}
            value={value}
          />
        );

        return (
          <div key={key} className="space-y-1.5">
            <label
              className="text-sm font-medium"
              htmlFor={`provider-cfg-${key}`}
            >
              {typeof prop.title === "string" ? prop.title : key}
              {required.has(key) ? (
                <span className="ml-1 text-destructive">*</span>
              ) : null}
            </label>
            {choices ? (
              <select
                className="flex h-9 w-full rounded-md border border-input bg-background px-3 py-2 text-sm shadow-xs"
                id={`provider-cfg-${key}`}
                onChange={(e) => {
                  const isOther =
                    e.target.value === PROVIDER_CONFIG_OTHER_VALUE;
                  setOther(key, isOther);
                  // Picking "Other…" keeps the current value so the free-text
                  // field opens on it — the common edit is a listed host with
                  // a different user or a suffix, not a blank restart.
                  if (!isOther) onChange({ ...config, [key]: e.target.value });
                }}
                value={freeText ? PROVIDER_CONFIG_OTHER_VALUE : value}
              >
                {value === "" && !freeText ? (
                  <option value="">Select</option>
                ) : null}
                {choices.map((choice) => (
                  <option key={choice.value} value={choice.value}>
                    {choice.label}
                  </option>
                ))}
                <option value={PROVIDER_CONFIG_OTHER_VALUE}>Other…</option>
              </select>
            ) : null}
            {freeText ? textInput : null}
            {description ? (
              <p className="text-xs text-muted-foreground">{description}</p>
            ) : null}
          </div>
        );
      })}
    </div>
  );
}
