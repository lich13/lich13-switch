export function routingReason(reason: string): string {
  return (
    (
      {
        thread_unidentified: "缺少可靠线程标识，按普通队列",
        new_thread: "识别为新线程",
        existing_thread: "维持原供应商",
        model_detour_owner_preserved: "模型绕行，线程归属保持",
        awaiting_compaction: "压缩尚未完成",
        compaction_window_mismatch: "压缩窗口未匹配，维持原供应商",
        cursor_pinned: "响应游标固定原供应商",
        no_eligible_receiver: "接收方暂不可用，维持原供应商",
        receiver_capacity_full: "接管名额已满，原供应商继续",
        boundary_in_flight: "其他请求正在接管",
        compaction_window_missing: "未携带压缩窗口，维持原供应商",
        compaction_handoff: "已尝试压缩后接管",
        boundary_not_observed: "未观察到可用压缩边界",
        boundary_consumed: "压缩窗口已消费，维持原供应商",
        auxiliary: "辅助请求，不建立线程归属",
        ordinary_queue: "按普通队列建立归属",
        first_turn_unconfirmed: "首轮证据不足，按普通队列",
        new_thread_capacity_full: "新线程接收方并发预留已满",
        new_thread_model_denied: "新线程接收方的白名单不匹配",
        new_thread_model_cooling: "新线程接收方的当前模型正在冷却",
        new_thread_provider_unavailable: "新线程接收方当前不可用",
        new_thread_queue_waiting: "新线程接收方已有等待请求",
      } as Record<string, string>
    )[reason] ?? "调度原因未记录"
  );
}
export function routingInput(value: string): string {
  return (
    (
      {
        first_turn: "已确认首轮输入",
        history: "包含历史消息或工具结果",
        response_cursor: "包含响应游标",
        compaction: "包含压缩边界或触发标记",
        warmup: "预热或空输入",
        missing_input: "缺少输入字段",
        unknown_structure: "输入结构无法识别",
        metadata_unavailable: "请求元数据无法读取",
      } as Record<string, string>
    )[value] ?? "未记录"
  );
}
export function routingRestriction(value: string): string {
  return (
    (
      {
        model_mismatch: "模型白名单不匹配",
        model_cooling: "当前模型正在冷却",
        provider_unavailable: "供应商健康状态不可用",
        capacity_full: "并发或线程预留名额已满",
        queue_waiting: "已有请求排队等待",
      } as Record<string, string>
    )[value] ?? "未记录"
  );
}

export function routingIdentity(value: string): string {
  return (
    (
      {
        reliable: "可靠线程标识",
        legacy_session: "旧式 session 回退",
        missing: "缺少线程标识",
        conflict: "线程标识冲突或无效",
      } as Record<string, string>
    )[value] ?? "线程身份未确认"
  );
}
export function routingBoundary(value: string): string {
  return (
    (
      {
        not_observed: "未观察到边界",
        fresh: "新鲜边界",
        claimed: "边界领取中",
        consumed: "边界已消费",
        mismatch: "边界匹配失败",
      } as Record<string, string>
    )[value] ?? "边界状态未确认"
  );
}
export function routingOutcome(value: string): string {
  return (
    (
      {
        attempted: "已尝试，尚未确认归属保存",
        owner_preserved: "维持线程归属",
        window_consumed: "真实生成已交付，旧窗口失效",
        window_released: "未输出，已释放并保存压缩机会",
        boundary_registered: "新压缩窗口已保存",
        ownership_saved: "归属已保存",
        save_failed: "归属保存失败",
        failed: "请求未成功完成",
      } as Record<string, string>
    )[value] ?? "路由结果未确认"
  );
}
export function routingFailure(value: string): string {
  return (
    (
      {
        rate_limit: "临时限流",
        account_rejected: "明确额度或账号拒绝",
        model_unavailable: "模型不可用",
        upstream_service: "上游服务故障",
        transport: "传输故障",
        protocol: "协议错误",
      } as Record<string, string>
    )[value] ?? "故障原因未确认"
  );
}
