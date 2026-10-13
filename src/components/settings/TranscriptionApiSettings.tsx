import React, { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  commands,
  events,
  type Result,
  type TranscriptionApiStatus,
} from "@/bindings";
import { useSettings } from "../../hooks/useSettings";
import { Alert } from "../ui/Alert";
import { Button } from "../ui/Button";
import { Input } from "../ui/Input";
import { SettingContainer } from "../ui/SettingContainer";
import { ToggleSwitch } from "../ui/ToggleSwitch";

interface Props {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

const DEFAULT_ADDRESS = "127.0.0.1";
const DEFAULT_PORT = 24639;

export const TranscriptionApiSettings: React.FC<Props> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, refreshSettings } = useSettings();
  const settings = getSetting("transcription_api");

  const [status, setStatus] = useState<TranscriptionApiStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [address, setAddress] = useState(
    settings?.bind_address ?? DEFAULT_ADDRESS,
  );
  const [port, setPort] = useState(String(settings?.port ?? DEFAULT_PORT));
  const [keepWarm, setKeepWarm] = useState(
    String(settings?.keep_warm_secs ?? 300),
  );
  const [confirmExpose, setConfirmExpose] = useState(false);
  const [token, setToken] = useState<string | null>(null);
  const [copied, setCopied] = useState<"url" | "token" | null>(null);

  useEffect(() => {
    let active = true;
    commands.getTranscriptionApiStatus().then((s) => active && setStatus(s));
    const unlisten = events.transcriptionApiStatus.listen((e) => {
      setStatus(e.payload);
    });
    return () => {
      active = false;
      unlisten.then((fn) => fn());
    };
  }, []);

  // Hide the token again whenever it could have changed or the page is left.
  useEffect(() => () => setToken(null), []);

  const enabled = settings?.enabled ?? false;
  const state = status?.state ?? "disabled";
  const isLoopback = (addr: string) =>
    addr === "::1" || addr.startsWith("127.");
  const wantsExposure = !isLoopback(address.trim());

  const run = useCallback(
    async (action: () => Promise<Result<TranscriptionApiStatus, string>>) => {
      setBusy(true);
      setError(null);
      try {
        const result = await action();
        if (result.status === "ok") {
          setStatus(result.data);
        } else {
          setError(result.error);
        }
      } catch (e) {
        setError(String(e));
      } finally {
        await refreshSettings();
        setBusy(false);
      }
    },
    [refreshSettings],
  );

  const copy = async (text: string, what: "url" | "token") => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(what);
      setTimeout(() => setCopied(null), 2000);
    } catch (e) {
      setError(String(e));
    }
  };

  const applyBinding = (allow: boolean) => {
    const portNumber = Number(port);
    if (!Number.isInteger(portNumber) || portNumber < 1 || portNumber > 65535) {
      setError(t("settings.advanced.transcriptionApi.invalidPort"));
      return;
    }
    setConfirmExpose(false);
    void run(() =>
      commands.setTranscriptionApiBinding(address.trim(), portNumber, allow),
    );
  };

  const onApplyClick = () => {
    if (wantsExposure) {
      setConfirmExpose(true);
    } else {
      applyBinding(false);
    }
  };

  const reveal = async () => {
    const result = await commands.revealTranscriptionApiToken();
    if (result.status === "ok") {
      setToken(result.data);
    } else {
      setError(result.error);
    }
  };

  const rotate = async () => {
    const result = await commands.rotateTranscriptionApiToken();
    if (result.status === "ok") {
      setToken(result.data);
      setError(null);
    } else {
      setError(result.error);
    }
  };

  const applyKeepWarm = async () => {
    const secs = Math.max(0, Math.floor(Number(keepWarm) || 0));
    setKeepWarm(String(secs));
    await commands.setTranscriptionApiKeepWarmSecs(secs);
    await refreshSettings();
  };

  const toggleDisabled = busy || state === "starting" || state === "stopping";

  return (
    <>
      <ToggleSwitch
        checked={enabled}
        onChange={(value) =>
          void run(() => commands.setTranscriptionApiEnabled(value))
        }
        isUpdating={busy}
        disabled={toggleDisabled}
        label={t("settings.advanced.transcriptionApi.enable.label")}
        description={t("settings.advanced.transcriptionApi.enable.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />

      <SettingContainer
        title={t("settings.advanced.transcriptionApi.status.label")}
        description={t("settings.advanced.transcriptionApi.stayRunning")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <span role="status" className="text-sm font-semibold">
          {t(`settings.advanced.transcriptionApi.status.${state}`)}
        </span>
      </SettingContainer>

      {state === "running" && status?.endpoint_url && (
        <SettingContainer
          title={t("settings.advanced.transcriptionApi.endpoint.label")}
          description={t(
            "settings.advanced.transcriptionApi.endpoint.description",
          )}
          descriptionMode={descriptionMode}
          grouped={grouped}
          layout="stacked"
        >
          <div className="flex items-center gap-2">
            <Input
              readOnly
              value={status.endpoint_url}
              aria-label={t(
                "settings.advanced.transcriptionApi.endpoint.label",
              )}
              className="flex-1 min-w-0"
              onFocus={(e) => e.currentTarget.select()}
            />
            <Button
              variant="secondary"
              size="sm"
              onClick={() =>
                status.endpoint_url && copy(status.endpoint_url, "url")
              }
            >
              {copied === "url"
                ? t("settings.advanced.transcriptionApi.copied")
                : t("settings.advanced.transcriptionApi.copy")}
            </Button>
          </div>
        </SettingContainer>
      )}

      {(error || status?.error) && (
        <Alert variant="error" contained>
          {error ?? status?.error}
        </Alert>
      )}

      <SettingContainer
        title={t("settings.advanced.transcriptionApi.binding.label")}
        description={t(
          "settings.advanced.transcriptionApi.binding.description",
        )}
        descriptionMode={descriptionMode}
        grouped={grouped}
        layout="stacked"
      >
        <div className="flex flex-wrap items-center gap-2">
          <Input
            value={address}
            onChange={(e) => {
              setAddress(e.target.value);
              setConfirmExpose(false);
            }}
            aria-label={t("settings.advanced.transcriptionApi.binding.address")}
            placeholder={DEFAULT_ADDRESS}
            spellCheck={false}
            className="w-44"
          />
          <Input
            value={port}
            onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))}
            aria-label={t("settings.advanced.transcriptionApi.binding.port")}
            inputMode="numeric"
            className="w-24"
          />
          <Button
            variant="primary-soft"
            size="sm"
            disabled={busy}
            onClick={onApplyClick}
          >
            {t("settings.advanced.transcriptionApi.binding.apply")}
          </Button>
        </div>
      </SettingContainer>

      {(status?.exposed || wantsExposure) && (
        <Alert variant="warning" contained>
          {t("settings.advanced.transcriptionApi.exposedWarning")}
        </Alert>
      )}

      {confirmExpose && (
        <div
          role="alertdialog"
          aria-label={t("settings.advanced.transcriptionApi.confirm.title")}
          className="flex flex-col gap-2 p-4 bg-yellow-500/10"
        >
          <p className="text-sm font-semibold">
            {t("settings.advanced.transcriptionApi.confirm.title")}
          </p>
          <p className="text-sm">
            {t("settings.advanced.transcriptionApi.confirm.body", { address })}
          </p>
          <div className="flex gap-2">
            <Button
              variant="danger"
              size="sm"
              onClick={() => applyBinding(true)}
            >
              {t("settings.advanced.transcriptionApi.confirm.accept")}
            </Button>
            <Button
              variant="secondary"
              size="sm"
              onClick={() => setConfirmExpose(false)}
            >
              {t("settings.advanced.transcriptionApi.confirm.cancel")}
            </Button>
          </div>
        </div>
      )}

      <SettingContainer
        title={t("settings.advanced.transcriptionApi.token.label")}
        description={t("settings.advanced.transcriptionApi.token.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
        layout="stacked"
      >
        <div className="flex flex-col gap-2">
          {token && (
            <Input
              readOnly
              value={token}
              aria-label={t("settings.advanced.transcriptionApi.token.label")}
              onFocus={(e) => e.currentTarget.select()}
              className="font-mono"
            />
          )}
          <div className="flex flex-wrap gap-2">
            {token ? (
              <>
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() => copy(token, "token")}
                >
                  {copied === "token"
                    ? t("settings.advanced.transcriptionApi.copied")
                    : t("settings.advanced.transcriptionApi.copy")}
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => setToken(null)}
                >
                  {t("settings.advanced.transcriptionApi.token.hide")}
                </Button>
              </>
            ) : (
              <Button variant="secondary" size="sm" onClick={reveal}>
                {t("settings.advanced.transcriptionApi.token.reveal")}
              </Button>
            )}
            <Button variant="danger-ghost" size="sm" onClick={rotate}>
              {t("settings.advanced.transcriptionApi.token.rotate")}
            </Button>
          </div>
        </div>
      </SettingContainer>

      <SettingContainer
        title={t("settings.advanced.transcriptionApi.keepWarm.label")}
        description={t(
          "settings.advanced.transcriptionApi.keepWarm.description",
        )}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="flex items-center gap-2">
          <Input
            value={keepWarm}
            onChange={(e) => setKeepWarm(e.target.value.replace(/\D/g, ""))}
            onBlur={() => void applyKeepWarm()}
            aria-label={t("settings.advanced.transcriptionApi.keepWarm.label")}
            inputMode="numeric"
            variant="compact"
            className="w-24"
          />
          <span className="text-sm">
            {t("settings.advanced.transcriptionApi.keepWarm.unit")}
          </span>
        </div>
      </SettingContainer>
    </>
  );
};
