import type { ComponentProps, JSX } from 'react';

import { Button } from '../../components/ui/button';
import { useTouchTap } from './useTouchTap';

/** A Button whose touch taps register on pointerup so two fingers can tap two
 *  buttons at once (see useTouchTap). `onTap` replaces `onClick`. */
export function TapButton({
  onTap,
  ...props
}: Omit<ComponentProps<typeof Button>, 'onClick'> & { readonly onTap: () => void }): JSX.Element {
  return <Button {...props} {...useTouchTap(onTap)} />;
}
