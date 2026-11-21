import React from "react"

import { PageHeader } from "../ui/page-header"

/** Card wrapper for a topology table section (title, subtitle, optional action). */
export const TopologySection: React.FC<{
  testId?: string
  icon: React.ReactNode
  title: string
  subtitle: string
  action?: React.ReactNode
  children: React.ReactNode
  footer?: React.ReactNode
}> = ({ testId, icon, title, subtitle, action, children, footer }) => (
  <section
    data-testid={testId}
    aria-label={title}
    className="bg-slate-900 border border-slate-800 rounded-2xl overflow-hidden"
  >
    <div className="space-y-4 p-4 sm:p-6">
      <PageHeader
        variant="section"
        inlineAt="xl"
        titleClassName="font-semibold"
        icon={icon}
        title={title}
        description={subtitle}
        actions={action}
      />
      {children}
    </div>
    {footer}
  </section>
)
