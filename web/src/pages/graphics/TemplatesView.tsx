// Graphics → Templates: the built-in template families with a sample render
// (bound to the first matching data source). Weather today; lower thirds,
// tickers and full-screen families register here later.
import type { JSX } from 'react';
import { Trans } from '@lingui/react/macro';
import { useQuery } from '@tanstack/react-query';

import { graphicsKeys, listTemplates, templateSamplePath } from '../../api/graphics';
import type { TemplateDoc } from '../../api/graphics';
import { Badge } from '../../components/ui/badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '../../components/ui/card';
import { useJpegPreview } from '../../preview/useJpegPreview';

function Sample({ template }: { readonly template: TemplateDoc }): JSX.Element {
  const url = useJpegPreview(templateSamplePath(template.id), 120_000);
  return (
    <div className="aspect-video w-full overflow-hidden rounded bg-[repeating-conic-gradient(#333_0_25%,#222_0_50%)] bg-[length:16px_16px]">
      {url !== undefined ? <img src={url} alt={template.name} className="h-full w-full object-contain" /> : null}
    </div>
  );
}

export function TemplatesView(): JSX.Element {
  const templates = useQuery({ queryKey: graphicsKeys.templates, queryFn: listTemplates });
  return (
    <div className="grid gap-4 md:grid-cols-2">
      {(templates.data ?? []).map((tpl) => (
        <Card key={tpl.id}>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              {tpl.name} <Badge variant="secondary">{tpl.family}</Badge>
            </CardTitle>
            <CardDescription>{tpl.description}</CardDescription>
          </CardHeader>
          <CardContent className="space-y-2 text-sm">
            <Sample template={tpl} />
            <div>
              <Trans>Data:</Trans> <code>{tpl.data_kind ?? '—'}</code>
            </div>
            <div>
              <Trans>Fields:</Trans> <code className="text-xs">{tpl.fields.join(', ')}</code>
            </div>
            <div>
              <Trans>Style options:</Trans> <code className="text-xs">{tpl.style_options.join(', ')}</code>
            </div>
          </CardContent>
        </Card>
      ))}
    </div>
  );
}
