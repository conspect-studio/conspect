import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { Dialog, DialogContent, DialogTitle, DialogTrigger } from './dialog';

describe('Dialog touch scroll opt-in', () => {
  it('keeps the default surface and close control unchanged', () => {
    render(<Dialog open><DialogContent aria-describedby={undefined}><DialogTitle>Default</DialogTitle></DialogContent></Dialog>);
    const dialog = screen.getByRole('dialog');
    expect(dialog).toHaveClass('top-1/2', '-translate-y-1/2');
    expect(dialog).not.toHaveClass('overflow-y-auto', 'overscroll-contain');
    expect(screen.getByRole('button', { name: 'Close' })).not.toHaveClass('size-12');
    expect(dialog).not.toHaveAttribute('touchScroll');
  });

  it('opts into one contained scroll surface and touch targets without changing Radix dismissal', async () => {
    const user = userEvent.setup();
    render(<Dialog><DialogTrigger>Open</DialogTrigger><DialogContent touchScroll aria-describedby={undefined}><DialogTitle>Touch</DialogTitle><input aria-label="Name" /><button>Save</button></DialogContent></Dialog>);
    await user.click(screen.getByRole('button', { name: 'Open' }));
    expect(screen.getByRole('dialog')).toHaveClass('max-h-[calc(100dvh-1rem)]', 'overflow-y-auto', 'overscroll-contain');
    expect(screen.getByRole('button', { name: 'Close' })).toHaveClass('size-12');
    expect(screen.getByRole('dialog')).not.toHaveAttribute('touchScroll');
    await user.keyboard('{Escape}');
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Open' })).toHaveFocus();
  });
});
